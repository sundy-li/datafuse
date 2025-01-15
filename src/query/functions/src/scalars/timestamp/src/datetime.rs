// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write;

use chrono::format::parse_and_remainder;
use chrono::format::Parsed;
use chrono::format::StrftimeItems;
use chrono::prelude::*;
use chrono::Datelike;
use chrono::TimeZone as ChronoTz;
use databend_common_exception::ErrorCode;
use databend_common_expression::error_to_null;
use databend_common_expression::types::date::clamp_date;
use databend_common_expression::types::date::date_to_string;
use databend_common_expression::types::date::string_to_date;
use databend_common_expression::types::date::DATE_MAX;
use databend_common_expression::types::date::DATE_MIN;
use databend_common_expression::types::nullable::NullableColumn;
use databend_common_expression::types::nullable::NullableDomain;
use databend_common_expression::types::number::Int64Type;
use databend_common_expression::types::number::SimpleDomain;
use databend_common_expression::types::number::UInt16Type;
use databend_common_expression::types::number::UInt32Type;
use databend_common_expression::types::number::UInt64Type;
use databend_common_expression::types::number::UInt8Type;
use databend_common_expression::types::string::StringDomain;
use databend_common_expression::types::timestamp::clamp_timestamp;
use databend_common_expression::types::timestamp::string_to_timestamp;
use databend_common_expression::types::timestamp::timestamp_to_string;
use databend_common_expression::types::timestamp::MICROS_PER_MILLI;
use databend_common_expression::types::timestamp::MICROS_PER_SEC;
use databend_common_expression::types::Bitmap;
use databend_common_expression::types::DateType;
use databend_common_expression::types::Float64Type;
use databend_common_expression::types::Int32Type;
use databend_common_expression::types::NullableType;
use databend_common_expression::types::NumberType;
use databend_common_expression::types::StringType;
use databend_common_expression::types::TimestampType;
use databend_common_expression::types::F64;
use databend_common_expression::utils::date_helper::*;
use databend_common_expression::utils::serialize::EPOCH_DAYS_FROM_CE;
use databend_common_expression::vectorize_1_arg;
use databend_common_expression::vectorize_2_arg;
use databend_common_expression::vectorize_with_builder_1_arg;
use databend_common_expression::vectorize_with_builder_2_arg;
use databend_common_expression::EvalContext;
use databend_common_expression::FunctionDomain;
use databend_common_expression::FunctionProperty;
use databend_common_expression::FunctionRegistry;
use databend_common_expression::Value;
use dtparse::parse;
use jiff::civil::date;
use jiff::civil::datetime;
use jiff::civil::Date;
use jiff::tz::Offset;
use jiff::tz::TimeZone;
use jiff::Unit;
use num_traits::AsPrimitive;

pub fn register(registry: &mut FunctionRegistry) {
    // cast(xx AS timestamp)
    // to_timestamp(xx)
    register_string_to_timestamp(registry);
    register_date_to_timestamp(registry);
    register_number_to_timestamp(registry);

    // cast(xx AS date)
    // to_date(xx)
    register_string_to_date(registry);
    register_timestamp_to_date(registry);
    register_number_to_date(registry);

    // cast([date | timestamp] AS string)
    // to_string([date | timestamp])
    register_to_string(registry);

    // cast([date | timestamp] AS [uint8 | int8 | ...])
    // to_[uint8 | int8 | ...]([date | timestamp])
    register_to_number(registry);

    // [add | subtract]_[years | months | days | hours | minutes | seconds]([date | timestamp], number)
    // date_[add | sub]([year | quarter | month | week | day | hour | minute | second], [date | timestamp], number)
    // [date | timestamp] [+ | -] interval number [year | quarter | month | week | day | hour | minute | second]
    register_add_functions(registry);
    register_sub_functions(registry);

    // date_diff([year | quarter | month | week | day | hour | minute | second], [date | timestamp], [date | timestamp])
    // [date | timestamp] +/- [date | timestamp]
    register_diff_functions(registry);

    // now, today, yesterday, tomorrow
    register_real_time_functions(registry);

    // to_*([date | timestamp]) -> number
    register_to_number_functions(registry);

    // to_*([date | timestamp]) -> [date | timestamp]
    register_rounder_functions(registry);

    // [date | timestamp] +/- number
    register_timestamp_add_sub(registry);

    // convert_timezone( target_timezone, 'timestamp')
    register_convert_timezone(registry);
}

/// Check if timestamp is within range, and return the timestamp in micros.
#[inline]
pub fn int64_to_timestamp(mut n: i64) -> i64 {
    if -31536000000 < n && n < 31536000000 {
        n * MICROS_PER_SEC
    } else if -31536000000000 < n && n < 31536000000000 {
        n * MICROS_PER_MILLI
    } else {
        clamp_timestamp(&mut n);
        n
    }
}

fn int64_domain_to_timestamp_domain<T: AsPrimitive<i64>>(
    domain: &SimpleDomain<T>,
) -> Option<SimpleDomain<i64>> {
    Some(SimpleDomain {
        min: int64_to_timestamp(domain.min.as_()),
        max: int64_to_timestamp(domain.max.as_()),
    })
}

fn register_convert_timezone(registry: &mut FunctionRegistry) {
    // 2 arguments function [target_timezone, src_timestamp]
    registry.register_passthrough_nullable_2_arg::<StringType, TimestampType, TimestampType, _, _>(
        "convert_timezone",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<StringType, TimestampType, TimestampType>(
            |target_tz, src_timestamp, output, ctx| {
                if let Some(validity) = &ctx.validity {
                    if !validity.get_bit(output.len()) {
                        output.push(0);
                        return;
                    }
                }
                // Convert source timestamp from source timezone to target timezone
                let p_src_timestamp = src_timestamp.to_timestamp(ctx.func_ctx.jiff_tz.clone());
                let src_dst_from_utc = p_src_timestamp.offset().seconds();

                let t_tz = match TimeZone::get(target_tz) {
                    Ok(tz) => tz,
                    Err(e) => {
                        ctx.set_error(
                            output.len(),
                            format!("cannot parse target `timezone`. {}", e),
                        );
                        output.push(0);
                        return;
                    }
                };

                let result_timestamp = p_src_timestamp
                    .with_time_zone(t_tz.clone())
                    .timestamp()
                    .as_microsecond();
                let target_dst_from_utc = p_src_timestamp
                    .with_time_zone(t_tz.clone())
                    .offset()
                    .seconds();
                let offset_as_micros_sec = (target_dst_from_utc - src_dst_from_utc) as i64;
                match offset_as_micros_sec.checked_mul(MICROS_PER_SEC) {
                    Some(offset) => match result_timestamp.checked_add(offset) {
                        Some(res) => output.push(res),
                        None => {
                            ctx.set_error(output.len(), "calc final time error".to_string());
                            output.push(0);
                        }
                    },
                    None => {
                        ctx.set_error(output.len(), "calc time offset error".to_string());
                        output.push(0);
                    }
                }
            },
        ),
    );
}

fn register_string_to_timestamp(registry: &mut FunctionRegistry) {
    registry.register_aliases("to_date", &["str_to_date", "date"]);
    registry.register_aliases("to_year", &["str_to_year", "year"]);
    registry.register_aliases("to_day_of_month", &["day", "dayofmonth"]);
    registry.register_aliases("to_day_of_year", &["dayofyear"]);
    registry.register_aliases("to_month", &["month"]);
    registry.register_aliases("to_quarter", &["quarter"]);
    registry.register_aliases("to_week_of_year", &["week", "weekofyear"]);

    registry.register_aliases("to_timestamp", &["to_datetime", "str_to_timestamp"]);
    registry.register_aliases("try_to_timestamp", &["try_to_datetime"]);

    registry.register_passthrough_nullable_1_arg::<StringType, TimestampType, _, _>(
        "to_timestamp",
        |_, _| FunctionDomain::MayThrow,
        eval_string_to_timestamp,
    );
    registry.register_combine_nullable_1_arg::<StringType, TimestampType, _, _>(
        "try_to_timestamp",
        |_, _| FunctionDomain::Full,
        error_to_null(eval_string_to_timestamp),
    );

    fn eval_string_to_timestamp(
        val: Value<StringType>,
        ctx: &mut EvalContext,
    ) -> Value<TimestampType> {
        vectorize_with_builder_1_arg::<StringType, TimestampType>(|val, output, ctx| {
            if ctx.func_ctx.enable_strict_datetime_parser {
                match string_to_timestamp(val, &ctx.func_ctx.jiff_tz) {
                    Ok(ts) => output.push(ts.timestamp().as_microsecond()),
                    Err(e) => {
                        ctx.set_error(
                            output.len(),
                            format!("cannot parse to type `TIMESTAMP`. {}", e),
                        );
                        output.push(0);
                    }
                }
            } else {
                match parse(val) {
                    Ok((naive_dt, parse_tz)) => {
                        let dt = datetime(
                            naive_dt.year() as i16,
                            naive_dt.month() as i8,
                            naive_dt.day() as i8,
                            naive_dt.hour() as i8,
                            naive_dt.minute() as i8,
                            naive_dt.second() as i8,
                            naive_dt.nanosecond() as i32,
                        );
                        let tz = if let Some(parse_tz) = parse_tz {
                            let offset = parse_tz.local_minus_utc();
                            let offset = Offset::from_seconds(offset).unwrap();
                            offset.to_time_zone()
                        } else {
                            ctx.func_ctx.jiff_tz.clone()
                        };
                        match dt.to_zoned(tz) {
                            Ok(res) => output.push(res.timestamp().as_microsecond()),
                            Err(e) => {
                                ctx.set_error(
                                    output.len(),
                                    format!("cannot parse to type `TIMESTAMP`. {}", e),
                                );
                                output.push(0);
                            }
                        }
                    }
                    Err(err) => {
                        ctx.set_error(
                            output.len(),
                            format!("cannot parse to type `TIMESTAMP`. {}", err),
                        );
                        output.push(0);
                    }
                }
            }
        })(val, ctx)
    }

    registry.register_combine_nullable_2_arg::<StringType, StringType, TimestampType, _, _>(
        "to_timestamp",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<StringType, StringType, NullableType<TimestampType>>(
            |timestamp, format, output, ctx| match string_to_format_timestamp(
                timestamp, format, ctx,
            ) {
                Ok((ts, need_null)) => {
                    if need_null {
                        output.push_null();
                    } else {
                        output.push(ts);
                    }
                }
                Err(e) => {
                    ctx.set_error(output.len(), e.to_string());
                    output.push_null();
                }
            },
        ),
    );

    registry.register_combine_nullable_2_arg::<StringType, StringType, TimestampType, _, _>(
        "try_to_timestamp",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<StringType, StringType, NullableType<TimestampType>>(
            |timestamp, format, output, ctx| match string_to_format_timestamp(
                timestamp, format, ctx,
            ) {
                Ok((ts, need_null)) => {
                    if need_null {
                        output.push_null();
                    } else {
                        output.push(ts);
                    }
                }
                Err(_) => {
                    output.push_null();
                }
            },
        ),
    );

    registry.register_combine_nullable_2_arg::<StringType, StringType, DateType, _, _>(
        "to_date",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<StringType, StringType, NullableType<DateType>>(
            |date, format, output, ctx| {
                if format.is_empty() {
                    output.push_null();
                } else {
                    match NaiveDate::parse_from_str(date, format) {
                        Ok(res) => {
                            output.push(res.num_days_from_ce() - EPOCH_DAYS_FROM_CE);
                        }
                        Err(e) => {
                            ctx.set_error(output.len(), e.to_string());
                            output.push_null();
                        }
                    }
                }
            },
        ),
    );

    registry.register_combine_nullable_2_arg::<StringType, StringType, DateType, _, _>(
        "try_to_date",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<StringType, StringType, NullableType<DateType>>(
            |date, format, output, _| {
                if format.is_empty() {
                    output.push_null();
                } else {
                    match NaiveDate::parse_from_str(date, format) {
                        Ok(res) => {
                            output.push(res.num_days_from_ce() - EPOCH_DAYS_FROM_CE);
                        }
                        Err(_) => {
                            output.push_null();
                        }
                    }
                }
            },
        ),
    );
}

fn string_to_format_timestamp(
    timestamp: &str,
    format: &str,
    ctx: &mut EvalContext,
) -> Result<(i64, bool), Box<ErrorCode>> {
    if format.is_empty() {
        return Ok((0, true));
    }
    // Parse with extra checks for timezone
    // %Z	ACST	Local time zone name. Skips all non-whitespace characters during parsing. Identical to %:z when formatting. 6
    // %z	+0930	Offset from the local time to UTC (with UTC being +0000).
    // %:z	+09:30	Same as %z but with a colon.
    // %::z	+09:30:00	Offset from the local time to UTC with seconds.
    // %:::z	+09	Offset from the local time to UTC without minutes.
    // %#z	+09	Parsing only: Same as %z but allows minutes to be missing or present.
    let timezone_strftime = ["%Z", "%z", "%:z", "%::z", "%:::z", "%#z"];
    let parse_tz = timezone_strftime
        .iter()
        .any(|&pattern| format.contains(pattern));
    if ctx.func_ctx.parse_datetime_ignore_remainder {
        let mut parsed = Parsed::new();
        if let Err(e) = parse_and_remainder(&mut parsed, timestamp, StrftimeItems::new(format)) {
            return Err(Box::new(ErrorCode::BadArguments(format!("{}", e))));
        }
        // Additional checks and adjustments for parsed timestamp
        // If parsed.timestamp is Some no need to pad default year.
        if parsed.timestamp.is_none() {
            if parsed.year.is_none() {
                parsed.year = Some(1970);
                parsed.year_div_100 = Some(19);
                parsed.year_mod_100 = Some(70);
            }
            if parsed.month.is_none() {
                parsed.month = Some(1);
            }
            if parsed.day.is_none() {
                parsed.day = Some(1);
            }
            if parsed.hour_div_12.is_none() && parsed.hour_mod_12.is_none() {
                parsed.hour_div_12 = Some(0);
                parsed.hour_mod_12 = Some(0);
            }
            if parsed.minute.is_none() {
                parsed.minute = Some(0);
            }
            if parsed.second.is_none() {
                parsed.second = Some(0);
            }
        }

        if parse_tz {
            parsed.offset.get_or_insert(0);
            parsed
                .to_datetime()
                .map(|res| (res.timestamp_micros(), false))
                .map_err(|err| Box::new(ErrorCode::BadArguments(format!("{err}"))))
        } else {
            parsed
                .to_naive_datetime_with_offset(0)
                .map_err(|err| Box::new(ErrorCode::BadArguments(format!("{err}"))))
                .and_then(|res| {
                    let dt = datetime(
                        res.year() as i16,
                        res.month() as i8,
                        res.day() as i8,
                        res.hour() as i8,
                        res.minute() as i8,
                        res.second() as i8,
                        res.nanosecond() as i32,
                    );
                    match dt.to_zoned(ctx.func_ctx.jiff_tz.clone()) {
                        Ok(res) => Ok((res.timestamp().as_microsecond(), false)),
                        Err(e) => Err(Box::new(ErrorCode::BadArguments(format!(
                            "Can not parse timestamp with error: {}",
                            e
                        )))),
                    }
                })
        }
    } else if parse_tz {
        DateTime::parse_from_str(timestamp, format)
            .map(|res| (res.timestamp_micros(), false))
            .map_err(|err| Box::new(ErrorCode::BadArguments(format!("{}", err))))
    } else {
        NaiveDateTime::parse_from_str(timestamp, format)
            .map_err(|err| Box::new(ErrorCode::BadArguments(format!("{}", err))))
            .and_then(|res| {
                let dt = datetime(
                    res.year() as i16,
                    res.month() as i8,
                    res.day() as i8,
                    res.hour() as i8,
                    res.minute() as i8,
                    res.second() as i8,
                    res.nanosecond() as i32,
                );
                match dt.to_zoned(ctx.func_ctx.jiff_tz.clone()) {
                    Ok(res) => Ok((res.timestamp().as_microsecond(), false)),
                    Err(e) => Err(Box::new(ErrorCode::BadArguments(format!(
                        "Can not parse timestamp with error: {}",
                        e
                    )))),
                }
            })
    }
}

fn register_date_to_timestamp(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_1_arg::<DateType, TimestampType, _, _>(
        "to_timestamp",
        |ctx, domain| {
            FunctionDomain::Domain(SimpleDomain {
                min: calc_date_to_timestamp(domain.min, ctx.jiff_tz.clone()),
                max: calc_date_to_timestamp(domain.max, ctx.jiff_tz.clone()),
            })
        },
        eval_date_to_timestamp,
    );
    registry.register_combine_nullable_1_arg::<DateType, TimestampType, _, _>(
        "try_to_timestamp",
        |ctx, domain| {
            FunctionDomain::Domain(NullableDomain {
                has_null: false,
                value: Some(Box::new(SimpleDomain {
                    min: calc_date_to_timestamp(domain.min, ctx.jiff_tz.clone()),
                    max: calc_date_to_timestamp(domain.max, ctx.jiff_tz.clone()),
                })),
            })
        },
        error_to_null(eval_date_to_timestamp),
    );

    fn eval_date_to_timestamp(val: Value<DateType>, ctx: &mut EvalContext) -> Value<TimestampType> {
        vectorize_with_builder_1_arg::<DateType, TimestampType>(|val, output, _| {
            output.push(calc_date_to_timestamp(val, ctx.func_ctx.jiff_tz.clone()));
        })(val, ctx)
    }

    fn calc_date_to_timestamp(val: i32, tz: TimeZone) -> i64 {
        let ts = (val as i64) * 24 * 3600 * MICROS_PER_SEC;

        let tz_offset_micros = tz
            .to_timestamp(date(1970, 1, 1).at(0, 0, 0, 0))
            .unwrap()
            .as_microsecond();
        ts + tz_offset_micros
    }
}

fn register_number_to_timestamp(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_1_arg::<Int64Type, TimestampType, _, _>(
        "to_timestamp",
        |_, domain| {
            int64_domain_to_timestamp_domain(domain)
                .map(FunctionDomain::Domain)
                .unwrap_or(FunctionDomain::MayThrow)
        },
        eval_number_to_timestamp,
    );
    registry.register_combine_nullable_1_arg::<Int64Type, TimestampType, _, _>(
        "try_to_timestamp",
        |_, domain| {
            if let Some(domain) = int64_domain_to_timestamp_domain(domain) {
                FunctionDomain::Domain(NullableDomain {
                    has_null: false,
                    value: Some(Box::new(domain)),
                })
            } else {
                FunctionDomain::Full
            }
        },
        error_to_null(eval_number_to_timestamp),
    );

    registry.register_passthrough_nullable_2_arg::<Int64Type, UInt64Type, TimestampType, _, _>(
        "to_timestamp",
        |_, _, _| FunctionDomain::Full,
        vectorize_with_builder_2_arg::<Int64Type, UInt64Type, TimestampType>(
            |val, scale, output, _| {
                let mut n = val * 10i64.pow(6 - scale.clamp(0, 6) as u32);
                clamp_timestamp(&mut n);
                output.push(n)
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<Int64Type, UInt64Type, TimestampType, _, _>(
        "try_to_timestamp",
        |_, _, _| FunctionDomain::Full,
        vectorize_with_builder_2_arg::<Int64Type, UInt64Type, TimestampType>(
            |val, scale, output, _| {
                let mut n = val * 10i64.pow(6 - scale.clamp(0, 6) as u32);
                clamp_timestamp(&mut n);
                output.push(n);
            },
        ),
    );

    fn eval_number_to_timestamp(
        val: Value<Int64Type>,
        ctx: &mut EvalContext,
    ) -> Value<TimestampType> {
        vectorize_with_builder_1_arg::<Int64Type, TimestampType>(|val, output, _| {
            let ts = int64_to_timestamp(val);
            output.push(ts);
        })(val, ctx)
    }
}

fn register_string_to_date(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_1_arg::<StringType, DateType, _, _>(
        "to_date",
        |_, _| FunctionDomain::MayThrow,
        eval_string_to_date,
    );
    registry.register_combine_nullable_1_arg::<StringType, DateType, _, _>(
        "try_to_date",
        |_, _| FunctionDomain::Full,
        error_to_null(eval_string_to_date),
    );

    fn eval_string_to_date(val: Value<StringType>, ctx: &mut EvalContext) -> Value<DateType> {
        vectorize_with_builder_1_arg::<StringType, DateType>(|val, output, ctx| {
            if ctx.func_ctx.enable_strict_datetime_parser {
                match string_to_date(val, &ctx.func_ctx.jiff_tz) {
                    Ok(d) => match d.since((Unit::Day, date(1970, 1, 1))) {
                        Ok(s) => output.push(s.get_days()),
                        Err(e) => {
                            ctx.set_error(
                                output.len(),
                                format!("cannot parse to type `DATE`. {}", e),
                            );
                            output.push(0);
                        }
                    },
                    Err(e) => {
                        ctx.set_error(output.len(), format!("cannot parse to type `DATE`. {}", e));
                        output.push(0);
                    }
                }
            } else {
                match parse(val) {
                    Ok((naive_dt, _)) => {
                        output.push(naive_dt.date().num_days_from_ce() - EPOCH_DAYS_FROM_CE);
                    }
                    Err(e) => {
                        ctx.set_error(output.len(), format!("cannot parse to type `DATE`. {}", e));
                        output.push(0);
                    }
                }
            }
        })(val, ctx)
    }
}

fn register_timestamp_to_date(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_1_arg::<TimestampType, DateType, _, _>(
        "to_date",
        |ctx, domain| {
            FunctionDomain::Domain(SimpleDomain {
                min: calc_timestamp_to_date(domain.min, ctx.jiff_tz.clone()),
                max: calc_timestamp_to_date(domain.max, ctx.jiff_tz.clone()),
            })
        },
        eval_timestamp_to_date,
    );
    registry.register_combine_nullable_1_arg::<TimestampType, DateType, _, _>(
        "try_to_date",
        |ctx, domain| {
            FunctionDomain::Domain(NullableDomain {
                has_null: false,
                value: Some(Box::new(SimpleDomain {
                    min: calc_timestamp_to_date(domain.min, ctx.jiff_tz.clone()),
                    max: calc_timestamp_to_date(domain.max, ctx.jiff_tz.clone()),
                })),
            })
        },
        error_to_null(eval_timestamp_to_date),
    );

    fn eval_timestamp_to_date(val: Value<TimestampType>, ctx: &mut EvalContext) -> Value<DateType> {
        vectorize_with_builder_1_arg::<TimestampType, DateType>(|val, output, ctx| {
            let tz = ctx.func_ctx.jiff_tz.clone();
            output.push(calc_timestamp_to_date(val, tz));
        })(val, ctx)
    }
    fn calc_timestamp_to_date(val: i64, tz: TimeZone) -> i32 {
        val.to_timestamp(tz)
            .date()
            .since((Unit::Day, Date::new(1970, 1, 1).unwrap()))
            .unwrap()
            .get_days()
    }
}

fn register_number_to_date(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_1_arg::<Int64Type, DateType, _, _>(
        "to_date",
        |_, domain| {
            let (domain, overflowing) = domain.overflow_cast_with_minmax(DATE_MIN, DATE_MAX);
            if overflowing {
                FunctionDomain::MayThrow
            } else {
                FunctionDomain::Domain(domain)
            }
        },
        eval_number_to_date,
    );
    registry.register_combine_nullable_1_arg::<Int64Type, DateType, _, _>(
        "try_to_date",
        |_, domain| {
            let (domain, overflowing) = domain.overflow_cast_with_minmax(DATE_MIN, DATE_MAX);
            FunctionDomain::Domain(NullableDomain {
                has_null: overflowing,
                value: Some(Box::new(domain)),
            })
        },
        error_to_null(eval_number_to_date),
    );

    fn eval_number_to_date(val: Value<Int64Type>, ctx: &mut EvalContext) -> Value<DateType> {
        vectorize_with_builder_1_arg::<Int64Type, DateType>(|val, output, _| {
            output.push(clamp_date(val))
        })(val, ctx)
    }
}

fn register_to_string(registry: &mut FunctionRegistry) {
    registry.register_aliases("to_string", &["date_format"]);
    registry.register_combine_nullable_2_arg::<TimestampType, StringType, StringType, _, _>(
        "to_string",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, StringType, NullableType<StringType>>(
            |micros, format, output, ctx| {
                if format.is_empty() {
                    output.push_null();
                } else {
                    // Can't use `tz.timestamp_nanos(self.as_() * 1000)` directly, is may cause multiply with overflow.
                    let (mut secs, mut nanos) =
                        (micros / MICROS_PER_SEC, (micros % MICROS_PER_SEC) * 1_000);
                    if nanos < 0 {
                        secs -= 1;
                        nanos += 1_000_000_000;
                    }
                    let ts = ctx.func_ctx.tz.timestamp_opt(secs, nanos as u32).unwrap();
                    // https://github.com/BurntSushi/jiff/issues/155
                    // ASCII is currently required in jiff crate
                    let res = ts.format(format).to_string();
                    output.push(&res);
                }
            },
        ),
    );

    registry.register_passthrough_nullable_1_arg::<DateType, StringType, _, _>(
        "to_string",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, StringType>(|val, output, ctx| {
            write!(
                output.row_buffer,
                "{}",
                date_to_string(val, &ctx.func_ctx.jiff_tz)
            )
            .unwrap();
            output.commit_row();
        }),
    );

    registry.register_passthrough_nullable_1_arg::<TimestampType, StringType, _, _>(
        "to_string",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<TimestampType, StringType>(|val, output, ctx| {
            write!(
                output.row_buffer,
                "{}",
                timestamp_to_string(val, &ctx.func_ctx.jiff_tz)
            )
            .unwrap();
            output.commit_row();
        }),
    );

    registry.register_combine_nullable_1_arg::<DateType, StringType, _, _>(
        "try_to_string",
        |_, _| {
            FunctionDomain::Domain(NullableDomain {
                has_null: false,
                value: Some(Box::new(StringDomain {
                    min: "".to_string(),
                    max: None,
                })),
            })
        },
        vectorize_with_builder_1_arg::<DateType, NullableType<StringType>>(|val, output, ctx| {
            write!(
                output.builder.row_buffer,
                "{}",
                date_to_string(val, &ctx.func_ctx.jiff_tz)
            )
            .unwrap();
            output.builder.commit_row();
            output.validity.push(true);
        }),
    );

    registry.register_combine_nullable_1_arg::<TimestampType, StringType, _, _>(
        "try_to_string",
        |_, _| {
            FunctionDomain::Domain(NullableDomain {
                has_null: false,
                value: Some(Box::new(StringDomain {
                    min: "".to_string(),
                    max: None,
                })),
            })
        },
        vectorize_with_builder_1_arg::<TimestampType, NullableType<StringType>>(
            |val, output, ctx| {
                write!(
                    output.builder.row_buffer,
                    "{}",
                    timestamp_to_string(val, &ctx.func_ctx.jiff_tz)
                )
                .unwrap();
                output.builder.commit_row();
                output.validity.push(true);
            },
        ),
    );
}

fn register_to_number(registry: &mut FunctionRegistry) {
    registry.register_1_arg::<DateType, NumberType<i64>, _, _>(
        "to_int64",
        |_, domain| FunctionDomain::Domain(domain.overflow_cast().0),
        |val, _| val as i64,
    );

    registry.register_passthrough_nullable_1_arg::<TimestampType, NumberType<i64>, _, _>(
        "to_int64",
        |_, domain| FunctionDomain::Domain(*domain),
        |val, _| match val {
            Value::Scalar(scalar) => Value::Scalar(scalar),
            Value::Column(col) => Value::Column(col),
        },
    );

    registry.register_combine_nullable_1_arg::<DateType, NumberType<i64>, _, _>(
        "try_to_int64",
        |_, domain| {
            FunctionDomain::Domain(NullableDomain {
                has_null: false,
                value: Some(Box::new(domain.overflow_cast().0)),
            })
        },
        |val, _| match val {
            Value::Scalar(scalar) => Value::Scalar(Some(scalar as i64)),
            Value::Column(col) => Value::Column(NullableColumn::new(
                col.iter().map(|val| *val as i64).collect(),
                Bitmap::new_constant(true, col.len()),
            )),
        },
    );

    registry.register_combine_nullable_1_arg::<TimestampType, NumberType<i64>, _, _>(
        "try_to_int64",
        |_, domain| {
            FunctionDomain::Domain(NullableDomain {
                has_null: false,
                value: Some(Box::new(*domain)),
            })
        },
        |val, _| match val {
            Value::Scalar(scalar) => Value::Scalar(Some(scalar)),
            Value::Column(col) => {
                let validity = Bitmap::new_constant(true, col.len());
                Value::Column(NullableColumn::new(col, validity))
            }
        },
    );
}

macro_rules! signed_ident {
    ($name: ident) => {
        -$name
    };
}

macro_rules! unsigned_ident {
    ($name: ident) => {
        $name
    };
}

macro_rules! impl_register_arith_functions {
    ($name: ident, $op: literal, $signed_wrapper: tt) => {
        fn $name(registry: &mut FunctionRegistry) {
            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
                concat!($op, "_years"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|date, delta, builder, ctx| {
                    match EvalYearsImpl::eval_date(date, ctx.func_ctx.jiff_tz.clone(), $signed_wrapper!{delta}) {
                        Ok(t) => builder.push(t),
                        Err(e) => {
                            ctx.set_error(builder.len(), e);
                            builder.push(0);
                        },
                    }
                }),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_years"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, ctx| {
                        match EvalYearsImpl::eval_timestamp(ts, ctx.func_ctx.jiff_tz.clone(), $signed_wrapper!{delta}) {
                            Ok(t) => builder.push(t),
                            Err(e) => {
                                ctx.set_error(builder.len(), e);
                                builder.push(0);
                            },
                        }
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
                concat!($op, "_quarters"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|date, delta, builder, ctx| {
                    match EvalMonthsImpl::eval_date(date, ctx.func_ctx.jiff_tz.clone(), $signed_wrapper!{delta} * 3) {
                        Ok(t) => builder.push(t),
                        Err(e) => {
                            ctx.set_error(builder.len(), e);
                            builder.push(0);
                        },
                    }
                }),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_quarters"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, ctx| {
                        match EvalMonthsImpl::eval_timestamp(ts, ctx.func_ctx.jiff_tz.clone(), $signed_wrapper!{delta} * 3) {
                            Ok(t) => builder.push(t),
                            Err(e) => {
                                ctx.set_error(builder.len(), e);
                                builder.push(0);
                            },
                        }
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
                concat!($op, "_months"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|date, delta, builder, ctx| {
                    match EvalMonthsImpl::eval_date(date, ctx.func_ctx.jiff_tz.clone(), $signed_wrapper!{delta}) {
                        Ok(t) => builder.push(t),
                        Err(e) => {
                            ctx.set_error(builder.len(), e);
                            builder.push(0);
                        },
                    }
                }),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_months"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, ctx| {
                        match EvalMonthsImpl::eval_timestamp(ts, ctx.func_ctx.jiff_tz.clone(), $signed_wrapper!{delta}) {
                            Ok(t) => builder.push(t),
                            Err(e) => {
                                ctx.set_error(builder.len(), e);
                                builder.push(0);
                            },
                        }
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
                concat!($op, "_days"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|date, delta, builder, _| {
                    builder.push(EvalDaysImpl::eval_date(date, $signed_wrapper!{delta}))
                }),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_days"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        builder.push(EvalDaysImpl::eval_timestamp(ts, $signed_wrapper!{delta}))
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
                concat!($op, "_weeks"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|date, delta, builder, _| {
                    let delta = 7 * delta;
                    builder.push(EvalDaysImpl::eval_date(date, $signed_wrapper!{delta}))
                }),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_weeks"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        let delta = 7 * delta;
                        builder.push(EvalDaysImpl::eval_timestamp(ts, $signed_wrapper!{delta}))
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, TimestampType, _, _>(
                concat!($op, "_hours"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        let val = (ts as i64) * 24 * 3600 * MICROS_PER_SEC;
                        builder.push(EvalTimesImpl::eval_timestamp(
                            val,
                            $signed_wrapper!{delta},
                            FACTOR_HOUR,
                        ));
                    },
                ),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_hours"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        builder.push(EvalTimesImpl::eval_timestamp(
                            ts,
                            $signed_wrapper!{delta},
                            FACTOR_HOUR,
                        ));
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, TimestampType, _, _>(
                concat!($op, "_minutes"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        let val = (ts as i64) * 24 * 3600 * MICROS_PER_SEC;
                        builder.push(EvalTimesImpl::eval_timestamp(
                            val,
                            $signed_wrapper!{delta},
                            FACTOR_MINUTE,
                        ))
                    },
                ),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_minutes"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        builder.push(EvalTimesImpl::eval_timestamp(
                            ts,
                            $signed_wrapper!{delta},
                            FACTOR_MINUTE,
                        ));
                    },
                ),
            );

            registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, TimestampType, _, _>(
                concat!($op, "_seconds"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<DateType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        let val = (ts as i64) * 24 * 3600 * MICROS_PER_SEC;
                        builder.push(EvalTimesImpl::eval_timestamp(
                            val,
                            $signed_wrapper!{delta},
                            FACTOR_SECOND,
                        ));
                    },
                ),
            );
            registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
                concat!($op, "_seconds"),

                |_, _, _| FunctionDomain::MayThrow,
                vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
                    |ts, delta, builder, _| {
                        builder.push(EvalTimesImpl::eval_timestamp(
                            ts,
                            $signed_wrapper!{delta},
                            FACTOR_SECOND,
                        ));
                    },
                ),
            );
        }
    };
}

impl_register_arith_functions!(register_add_functions, "add", unsigned_ident);
impl_register_arith_functions!(register_sub_functions, "subtract", signed_ident);

fn register_diff_functions(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_2_arg::<DateType, DateType, Int64Type, _, _>(
        "diff_years",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<DateType, DateType, Int64Type>(
            |date_end, date_start, builder, ctx| {
                let diff_years = EvalYearsImpl::eval_date_diff(
                    date_start,
                    date_end,
                    ctx.func_ctx.jiff_tz.clone(),
                );
                builder.push(diff_years as i64);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_years",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, ctx| {
                let diff_years = EvalYearsImpl::eval_timestamp_diff(
                    date_start,
                    date_end,
                    ctx.func_ctx.jiff_tz.clone(),
                );
                builder.push(diff_years);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<DateType, DateType, Int64Type, _, _>(
        "diff_quarters",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<DateType, DateType, Int64Type>(
            |date_end, date_start, builder, ctx| {
                let diff_years = EvalQuartersImpl::eval_date_diff(
                    date_start,
                    date_end,
                    ctx.func_ctx.jiff_tz.clone(),
                );
                builder.push(diff_years as i64);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_quarters",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, ctx| {
                let diff_years = EvalQuartersImpl::eval_timestamp_diff(
                    date_start,
                    date_end,
                    ctx.func_ctx.jiff_tz.clone(),
                );
                builder.push(diff_years);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<DateType, DateType, Int64Type, _, _>(
        "diff_months",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<DateType, DateType, Int64Type>(
            |date_end, date_start, builder, ctx| {
                let diff_months = EvalMonthsImpl::eval_date_diff(
                    date_start,
                    date_end,
                    ctx.func_ctx.jiff_tz.clone(),
                );
                builder.push(diff_months as i64);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_months",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, ctx| {
                let diff_months = EvalMonthsImpl::eval_timestamp_diff(
                    date_start,
                    date_end,
                    ctx.func_ctx.jiff_tz.clone(),
                );
                builder.push(diff_months);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<DateType, DateType, Int64Type, _, _>(
        "diff_weeks",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<DateType, DateType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_years = EvalWeeksImpl::eval_date_diff(date_start, date_end);
                builder.push(diff_years as i64);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_weeks",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_years = EvalWeeksImpl::eval_timestamp_diff(date_start, date_end);
                builder.push(diff_years);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<DateType, DateType, Int64Type, _, _>(
        "diff_days",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<DateType, DateType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_days = EvalDaysImpl::eval_date_diff(date_start, date_end);
                builder.push(diff_days as i64);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_days",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_days = EvalDaysImpl::eval_timestamp_diff(date_start, date_end);
                builder.push(diff_days);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_hours",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_hours =
                    EvalTimesImpl::eval_timestamp_diff(date_start, date_end, FACTOR_HOUR);
                builder.push(diff_hours);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_minutes",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_minutes =
                    EvalTimesImpl::eval_timestamp_diff(date_start, date_end, FACTOR_MINUTE);
                builder.push(diff_minutes);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "diff_seconds",
        |_, _, _| FunctionDomain::MayThrow,
        vectorize_with_builder_2_arg::<TimestampType, TimestampType, Int64Type>(
            |date_end, date_start, builder, _| {
                let diff_seconds =
                    EvalTimesImpl::eval_timestamp_diff(date_start, date_end, FACTOR_SECOND);
                builder.push(diff_seconds);
            },
        ),
    );

    registry.register_2_arg::<DateType, DateType, Int32Type, _, _>(
        "minus",
        |_, lhs, rhs| {
            (|| {
                let lm = lhs.max;
                let ln = lhs.min;
                let rm: i32 = num_traits::cast::cast(rhs.max)?;
                let rn: i32 = num_traits::cast::cast(rhs.min)?;

                Some(FunctionDomain::Domain(SimpleDomain::<i32> {
                    min: ln.checked_sub(rm)?,
                    max: lm.checked_sub(rn)?,
                }))
            })()
            .unwrap_or(FunctionDomain::Full)
        },
        |a, b, _| a - b,
    );

    registry.register_2_arg::<TimestampType, TimestampType, Int64Type, _, _>(
        "minus",
        |_, lhs, rhs| {
            (|| {
                let lm = lhs.max;
                let ln = lhs.min;
                let rm = rhs.max;
                let rn = rhs.min;

                Some(FunctionDomain::Domain(SimpleDomain::<i64> {
                    min: ln.checked_sub(rm)?,
                    max: lm.checked_sub(rn)?,
                }))
            })()
            .unwrap_or(FunctionDomain::Full)
        },
        |a, b, _| a - b,
    );

    registry.register_passthrough_nullable_2_arg::<DateType, DateType, Float64Type, _, _>(
        "months_between",
        |_, lhs, rhs| {
            let lm = lhs.max;
            let ln = lhs.min;
            let rm = rhs.max;
            let rn = rhs.min;

            FunctionDomain::Domain(SimpleDomain::<F64> {
                min: EvalMonthsImpl::months_between(ln, rm).into(),
                max: EvalMonthsImpl::months_between(lm, rn).into(),
            })
        },
        vectorize_2_arg::<DateType, DateType, Float64Type>(|a, b, _ctx| {
            EvalMonthsImpl::months_between(a, b).into()
        }),
    );

    registry
        .register_passthrough_nullable_2_arg::<TimestampType, TimestampType, Float64Type, _, _>(
            "months_between",
            |_, lhs, rhs| {
                let lm = lhs.max;
                let ln = lhs.min;
                let rm = rhs.max;
                let rn = rhs.min;

                FunctionDomain::Domain(SimpleDomain::<F64> {
                    min: EvalMonthsImpl::months_between_ts(ln, rm).into(),
                    max: EvalMonthsImpl::months_between_ts(lm, rn).into(),
                })
            },
            vectorize_2_arg::<TimestampType, TimestampType, Float64Type>(|a, b, _ctx| {
                EvalMonthsImpl::months_between_ts(a, b).into()
            }),
        );
}

fn register_real_time_functions(registry: &mut FunctionRegistry) {
    registry.register_aliases("now", &["current_timestamp"]);

    registry.properties.insert(
        "now".to_string(),
        FunctionProperty::default().non_deterministic(),
    );
    registry.properties.insert(
        "today".to_string(),
        FunctionProperty::default().non_deterministic(),
    );
    registry.properties.insert(
        "yesterday".to_string(),
        FunctionProperty::default().non_deterministic(),
    );
    registry.properties.insert(
        "tomorrow".to_string(),
        FunctionProperty::default().non_deterministic(),
    );

    registry.register_0_arg_core::<TimestampType, _, _>(
        "now",
        |_| FunctionDomain::Full,
        |ctx| Value::Scalar(ctx.func_ctx.now.timestamp().as_microsecond()),
    );

    registry.register_0_arg_core::<DateType, _, _>(
        "today",
        |_| FunctionDomain::Full,
        |ctx| Value::Scalar(today_date(&ctx.func_ctx.now, &ctx.func_ctx.jiff_tz)),
    );

    registry.register_0_arg_core::<DateType, _, _>(
        "yesterday",
        |_| FunctionDomain::Full,
        |ctx| Value::Scalar(today_date(&ctx.func_ctx.now, &ctx.func_ctx.jiff_tz) - 1),
    );

    registry.register_0_arg_core::<DateType, _, _>(
        "tomorrow",
        |_| FunctionDomain::Full,
        |ctx| Value::Scalar(today_date(&ctx.func_ctx.now, &ctx.func_ctx.jiff_tz) + 1),
    );
}

fn register_to_number_functions(registry: &mut FunctionRegistry) {
    // date
    registry.register_passthrough_nullable_1_arg::<DateType, UInt32Type, _, _>(
        "to_yyyymm",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt32Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToYYYYMM, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt32Type, _, _>(
        "to_yyyymmdd",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt32Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToYYYYMMDD, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt64Type, _, _>(
        "to_yyyymmddhh",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt64Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToYYYYMMDDHH, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt64Type, _, _>(
        "to_yyyymmddhhmmss",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt64Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToYYYYMMDDHHMMSS, _>(val, ctx.func_ctx.jiff_tz.clone())
            {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt16Type, _, _>(
        "to_year",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt16Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToYear, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt8Type, _, _>(
        "to_quarter",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt8Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToQuarter, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt8Type, _, _>(
        "to_month",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt8Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToMonth, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt16Type, _, _>(
        "to_day_of_year",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt16Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToDayOfYear, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt8Type, _, _>(
        "to_day_of_month",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt8Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToDayOfMonth, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt8Type, _, _>(
        "to_day_of_week",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt8Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToDayOfWeek, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<DateType, UInt32Type, _, _>(
        "to_week_of_year",
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, UInt32Type>(|val, output, ctx| {
            match ToNumberImpl::eval_date::<ToWeekOfYear, _>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    // timestamp
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt32Type, _, _>(
        "to_yyyymm",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt32Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToYYYYMM, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt32Type, _, _>(
        "to_yyyymmdd",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt32Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToYYYYMMDD, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt64Type, _, _>(
        "to_yyyymmddhh",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt64Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToYYYYMMDDHH, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt64Type, _, _>(
        "to_yyyymmddhhmmss",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt64Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToYYYYMMDDHHMMSS, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt16Type, _, _>(
        "to_year",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt16Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToYear, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_quarter",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToQuarter, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_month",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToMonth, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt16Type, _, _>(
        "to_day_of_year",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt16Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToDayOfYear, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_day_of_month",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToDayOfMonth, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_day_of_week",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToDayOfWeek, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt32Type, _, _>(
        "to_week_of_year",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt32Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToWeekOfYear, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, Int64Type, _, _>(
        "to_unix_timestamp",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, Int64Type>(|val, ctx| {
            ToNumberImpl::eval_timestamp::<ToUnixTimestamp, _>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );

    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_hour",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            let datetime = val.to_timestamp(ctx.func_ctx.jiff_tz.clone());
            datetime.hour() as u8
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_minute",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            let datetime = val.to_timestamp(ctx.func_ctx.jiff_tz.clone());
            datetime.minute() as u8
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, UInt8Type, _, _>(
        "to_second",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, UInt8Type>(|val, ctx| {
            let datetime = val.to_timestamp(ctx.func_ctx.jiff_tz.clone());
            datetime.second() as u8
        }),
    );
}

fn register_timestamp_add_sub(registry: &mut FunctionRegistry) {
    registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
        "plus",
        |_, lhs, rhs| {
            (|| {
                let lm: i64 = num_traits::cast::cast(lhs.max)?;
                let ln: i64 = num_traits::cast::cast(lhs.min)?;
                let rm = rhs.max;
                let rn = rhs.min;

                Some(FunctionDomain::Domain(SimpleDomain::<i32> {
                    min: clamp_date(ln + rn),
                    max: clamp_date(lm + rm),
                }))
            })()
            .unwrap_or(FunctionDomain::MayThrow)
        },
        vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|a, b, output, _| {
            output.push(clamp_date((a as i64) + b))
        }),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
        "plus",
        |_, lhs, rhs| {
            {
                let lm = lhs.max;
                let ln = lhs.min;
                let rm = rhs.max;
                let rn = rhs.min;
                let mut min = ln + rn;
                clamp_timestamp(&mut min);
                let mut max = lm + rm;
                clamp_timestamp(&mut max);
                Some(FunctionDomain::Domain(SimpleDomain::<i64> { min, max }))
            }
            .unwrap_or(FunctionDomain::MayThrow)
        },
        vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
            |a, b, output, _| {
                let mut sum = a + b;
                clamp_timestamp(&mut sum);
                output.push(sum);
            },
        ),
    );

    registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
        "minus",
        |_, lhs, rhs| {
            (|| {
                let lm: i64 = num_traits::cast::cast(lhs.max)?;
                let ln: i64 = num_traits::cast::cast(lhs.min)?;
                let rm = rhs.max;
                let rn = rhs.min;

                Some(FunctionDomain::Domain(SimpleDomain::<i32> {
                    min: clamp_date(ln - rn),
                    max: clamp_date(lm - rm),
                }))
            })()
            .unwrap_or(FunctionDomain::MayThrow)
        },
        vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|a, b, output, _| {
            output.push(clamp_date((a as i64) - b));
        }),
    );

    registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, TimestampType, _, _>(
        "minus",
        |_, lhs, rhs| {
            {
                let lm = lhs.max;
                let ln = lhs.min;
                let rm = rhs.max;
                let rn = rhs.min;
                let mut min = ln - rn;
                clamp_timestamp(&mut min);
                let mut max = lm - rm;
                clamp_timestamp(&mut max);
                Some(FunctionDomain::Domain(SimpleDomain::<i64> { min, max }))
            }
            .unwrap_or(FunctionDomain::MayThrow)
        },
        vectorize_with_builder_2_arg::<TimestampType, Int64Type, TimestampType>(
            |a, b, output, _| {
                let mut minus = a - b;
                clamp_timestamp(&mut minus);
                output.push(minus);
            },
        ),
    );
}

fn register_rounder_functions(registry: &mut FunctionRegistry) {
    // timestamp -> timestamp
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_second",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::Second)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_minute",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::Minute)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_five_minutes",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::FiveMinutes)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_ten_minutes",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::TenMinutes)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_fifteen_minutes",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::FifteenMinutes)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_hour",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::Hour)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "to_start_of_day",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::Day)
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, TimestampType, _, _>(
        "time_slot",
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, TimestampType>(|val, ctx| {
            round_timestamp(val, &ctx.func_ctx.jiff_tz, Round::TimeSlot)
        }),
    );

    // date | timestamp -> date
    rounder_functions_helper::<ToLastMonday>(registry, "to_monday");
    rounder_functions_helper::<ToLastSunday>(registry, "to_start_of_week");
    rounder_functions_helper::<ToStartOfMonth>(registry, "to_start_of_month");
    rounder_functions_helper::<ToStartOfQuarter>(registry, "to_start_of_quarter");
    rounder_functions_helper::<ToStartOfYear>(registry, "to_start_of_year");
    rounder_functions_helper::<ToStartOfISOYear>(registry, "to_start_of_iso_year");
    rounder_functions_helper::<ToLastOfWeek>(registry, "to_last_of_week");
    rounder_functions_helper::<ToLastOfMonth>(registry, "to_last_of_month");
    rounder_functions_helper::<ToLastOfQuarter>(registry, "to_last_of_quarter");
    rounder_functions_helper::<ToLastOfYear>(registry, "to_last_of_year");
    rounder_functions_helper::<ToPreviousMonday>(registry, "to_previous_monday");
    rounder_functions_helper::<ToPreviousTuesday>(registry, "to_previous_tuesday");
    rounder_functions_helper::<ToPreviousWednesday>(registry, "to_previous_wednesday");
    rounder_functions_helper::<ToPreviousThursday>(registry, "to_previous_thursday");
    rounder_functions_helper::<ToPreviousFriday>(registry, "to_previous_friday");
    rounder_functions_helper::<ToPreviousSaturday>(registry, "to_previous_saturday");
    rounder_functions_helper::<ToPreviousSunday>(registry, "to_previous_sunday");
    rounder_functions_helper::<ToNextMonday>(registry, "to_next_monday");
    rounder_functions_helper::<ToNextTuesday>(registry, "to_next_tuesday");
    rounder_functions_helper::<ToNextWednesday>(registry, "to_next_wednesday");
    rounder_functions_helper::<ToNextThursday>(registry, "to_next_thursday");
    rounder_functions_helper::<ToNextFriday>(registry, "to_next_friday");
    rounder_functions_helper::<ToNextSaturday>(registry, "to_next_saturday");
    rounder_functions_helper::<ToNextSunday>(registry, "to_next_sunday");

    registry.register_passthrough_nullable_2_arg::<DateType, Int64Type, DateType, _, _>(
        "to_start_of_week",
        |_, _, _| FunctionDomain::Full,
        vectorize_with_builder_2_arg::<DateType, Int64Type, DateType>(|val, mode, output, ctx| {
            if mode == 0 {
                match DateRounder::eval_date::<ToLastSunday>(val, ctx.func_ctx.jiff_tz.clone()) {
                    Ok(t) => output.push(t),
                    Err(e) => {
                        ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                        output.push(0);
                    }
                }
            } else {
                match DateRounder::eval_date::<ToLastMonday>(val, ctx.func_ctx.jiff_tz.clone()) {
                    Ok(t) => output.push(t),
                    Err(e) => {
                        ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                        output.push(0);
                    }
                }
            }
        }),
    );
    registry.register_passthrough_nullable_2_arg::<TimestampType, Int64Type, DateType, _, _>(
        "to_start_of_week",
        |_, _, _| FunctionDomain::Full,
        vectorize_2_arg::<TimestampType, Int64Type, DateType>(|val, mode, ctx| {
            if mode == 0 {
                DateRounder::eval_timestamp::<ToLastSunday>(val, ctx.func_ctx.jiff_tz.clone())
            } else {
                DateRounder::eval_timestamp::<ToLastMonday>(val, ctx.func_ctx.jiff_tz.clone())
            }
        }),
    );
}

fn rounder_functions_helper<T>(registry: &mut FunctionRegistry, name: &str)
where T: ToNumber<i32> {
    registry.register_passthrough_nullable_1_arg::<DateType, DateType, _, _>(
        name,
        |_, _| FunctionDomain::Full,
        vectorize_with_builder_1_arg::<DateType, DateType>(|val, output, ctx| {
            match DateRounder::eval_date::<T>(val, ctx.func_ctx.jiff_tz.clone()) {
                Ok(t) => output.push(t),
                Err(e) => {
                    ctx.set_error(output.len(), format!("cannot parse to type `Date`. {}", e));
                    output.push(0);
                }
            }
        }),
    );
    registry.register_passthrough_nullable_1_arg::<TimestampType, DateType, _, _>(
        name,
        |_, _| FunctionDomain::Full,
        vectorize_1_arg::<TimestampType, DateType>(|val, ctx| {
            DateRounder::eval_timestamp::<T>(val, ctx.func_ctx.jiff_tz.clone())
        }),
    );
}