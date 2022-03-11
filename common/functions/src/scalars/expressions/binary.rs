// Copyright 2021 Datafuse Labs.
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

use std::marker::PhantomData;

use common_arrow::arrow::bitmap::MutableBitmap;
use common_arrow::arrow::compute::comparison::Simd8;
use common_arrow::arrow::compute::comparison::Simd8Lanes;
use common_datavalues::prelude::*;
use common_exception::Result;

use super::EvalContext;

pub trait ScalarBinaryFunction<L: Scalar, R: Scalar, O: Scalar> {
    fn eval(&self, l: L::RefType<'_>, r: R::RefType<'_>, ctx: &mut EvalContext) -> O;
}

/// Blanket implementation for all binary expression functions
impl<L: Scalar, R: Scalar, O: Scalar, F> ScalarBinaryFunction<L, R, O> for F
where F: Fn(L::RefType<'_>, R::RefType<'_>, &mut EvalContext) -> O
{
    fn eval(&self, i1: L::RefType<'_>, i2: R::RefType<'_>, ctx: &mut EvalContext) -> O {
        self(i1, i2, ctx)
    }
}

/// A common struct to caculate binary expression scalar op.
#[derive(Clone)]
pub struct ScalarBinaryExpression<L: Scalar, R: Scalar, O: Scalar, F> {
    func: F,
    _phantom: PhantomData<(L, R, O)>,
}

impl<L: Scalar, R: Scalar, O: Scalar, F> ScalarBinaryExpression<L, R, O, F>
where F: ScalarBinaryFunction<L, R, O>
{
    /// Create a binary expression from generic columns  and a lambda function.
    pub fn new(func: F) -> Self {
        Self {
            func,
            _phantom: PhantomData,
        }
    }

    /// Evaluate the expression with the given array.
    pub fn eval(
        &self,
        l: &ColumnRef,
        r: &ColumnRef,
        ctx: &mut EvalContext,
    ) -> Result<<O as Scalar>::ColumnType> {
        debug_assert!(
            l.len() == r.len(),
            "Size of columns must match to apply binary expression"
        );

        let result = match (l.is_const(), r.is_const()) {
            (false, true) => {
                let left: &<L as Scalar>::ColumnType = unsafe { Series::static_cast(l) };
                let right = R::try_create_viewer(r)?;

                let b = right.value_at(0);
                let it = left.scalar_iter().map(|a| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_owned_iterator(it)
            }

            (false, false) => {
                let left: &<L as Scalar>::ColumnType = unsafe { Series::static_cast(l) };
                let right: &<R as Scalar>::ColumnType = unsafe { Series::static_cast(r) };

                let it = left
                    .scalar_iter()
                    .zip(right.scalar_iter())
                    .map(|(a, b)| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_owned_iterator(it)
            }

            (true, false) => {
                let left = L::try_create_viewer(l)?;
                let a = left.value_at(0);

                let right: &<R as Scalar>::ColumnType = unsafe { Series::static_cast(r) };
                let it = right.scalar_iter().map(|b| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_owned_iterator(it)
            }

            // True True ?
            (true, true) => {
                let left = L::try_create_viewer(l)?;
                let right = R::try_create_viewer(r)?;

                let it = left
                    .iter()
                    .zip(right.iter())
                    .map(|(a, b)| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_owned_iterator(it)
            }
        };

        if let Some(error) = ctx.error.take() {
            return Err(error);
        }
        Ok(result)
    }
}

pub trait ScalarBinaryRefFunction<'a, L: Scalar, R: Scalar, O: Scalar> {
    fn eval(&self, l: L::RefType<'a>, r: R::RefType<'a>, ctx: &mut EvalContext) -> O::RefType<'a>;
}

/// Blanket implementation for all binary expression functions
impl<'a, L: Scalar, R: Scalar, O: Scalar, F> ScalarBinaryRefFunction<'a, L, R, O> for F
where F: Fn(L::RefType<'a>, R::RefType<'a>, &mut EvalContext) -> O::RefType<'a>
{
    fn eval(
        &self,
        i1: L::RefType<'a>,
        i2: R::RefType<'a>,
        ctx: &mut EvalContext,
    ) -> O::RefType<'a> {
        self(i1, i2, ctx)
    }
}

impl<'a, L: Scalar, R: Scalar, O: Scalar, F> ScalarBinaryExpression<L, R, O, F>
where F: ScalarBinaryRefFunction<'a, L, R, O>
{
    /// Create a binary expression from generic columns  and a lambda function.
    pub fn new_ref(func: F) -> Self {
        Self {
            func,
            _phantom: PhantomData,
        }
    }

    /// Evaluate the expression with the given array.
    pub fn eval_ref(
        &self,
        l: &'a ColumnRef,
        r: &'a ColumnRef,
        ctx: &mut EvalContext,
    ) -> Result<<O as Scalar>::ColumnType> {
        debug_assert!(
            l.len() == r.len(),
            "Size of columns must match to apply binary expression"
        );

        let result = match (l.is_const(), r.is_const()) {
            (false, true) => {
                let left: &<L as Scalar>::ColumnType = unsafe { Series::static_cast(l) };
                let right = R::try_create_viewer(r)?;

                let b = right.value_at(0);
                let it = left.scalar_iter().map(|a| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_iterator(it)
            }

            (false, false) => {
                let left: &<L as Scalar>::ColumnType = unsafe { Series::static_cast(l) };
                let right: &<R as Scalar>::ColumnType = unsafe { Series::static_cast(r) };

                let it = left
                    .scalar_iter()
                    .zip(right.scalar_iter())
                    .map(|(a, b)| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_iterator(it)
            }

            (true, false) => {
                let left = L::try_create_viewer(l)?;
                let a = left.value_at(0);

                let right: &<R as Scalar>::ColumnType = unsafe { Series::static_cast(r) };
                let it = right.scalar_iter().map(|b| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_iterator(it)
            }

            // True True ?
            (true, true) => {
                let left = L::try_create_viewer(l)?;
                let right = R::try_create_viewer(r)?;

                let it = left
                    .iter()
                    .zip(right.iter())
                    .map(|(a, b)| self.func.eval(a, b, ctx));
                <O as Scalar>::ColumnType::from_iterator(it)
            }
        };

        if let Some(error) = ctx.error.take() {
            return Err(error);
        }
        Ok(result)
    }
}

pub fn scalar_binary_op<L: Scalar, R: Scalar, O: Scalar, F>(
    l: &ColumnRef,
    r: &ColumnRef,
    f: F,
    ctx: &mut EvalContext,
) -> Result<<O as Scalar>::ColumnType>
where
    F: Fn(L::RefType<'_>, R::RefType<'_>, &mut EvalContext) -> O,
{
    debug_assert!(
        l.len() == r.len(),
        "Size of columns must match to apply binary expression"
    );

    let result = match (l.is_const(), r.is_const()) {
        (false, true) => {
            let left: &<L as Scalar>::ColumnType = unsafe { Series::static_cast(l) };
            let right = R::try_create_viewer(r)?;

            let b = right.value_at(0);
            let it = left.scalar_iter().map(|a| f(a, b, ctx));
            <O as Scalar>::ColumnType::from_owned_iterator(it)
        }

        (false, false) => {
            let left: &<L as Scalar>::ColumnType = unsafe { Series::static_cast(l) };
            let right: &<R as Scalar>::ColumnType = unsafe { Series::static_cast(r) };

            let it = left
                .scalar_iter()
                .zip(right.scalar_iter())
                .map(|(a, b)| f(a, b, ctx));
            <O as Scalar>::ColumnType::from_owned_iterator(it)
        }

        (true, false) => {
            let left = L::try_create_viewer(l)?;
            let a = left.value_at(0);

            let right: &<R as Scalar>::ColumnType = unsafe { Series::static_cast(r) };
            let it = right.scalar_iter().map(|b| f(a, b, ctx));
            <O as Scalar>::ColumnType::from_owned_iterator(it)
        }

        // True True ?
        (true, true) => {
            let left = L::try_create_viewer(l)?;
            let right = R::try_create_viewer(r)?;

            let it = left.iter().zip(right.iter()).map(|(a, b)| f(a, b, ctx));
            <O as Scalar>::ColumnType::from_owned_iterator(it)
        }
    };

    if let Some(error) = ctx.error.take() {
        return Err(error);
    }
    Ok(result)
}

/// QUOTE: (From arrow2::arrow::compute::comparison::primitive)
pub fn primitive_simd_op_boolean<T, F>(l: &ColumnRef, r: &ColumnRef, op: F) -> Result<BooleanColumn>
where
    T: PrimitiveType + Simd8,
    F: Fn(T::Simd, T::Simd) -> u8,
{
    debug_assert!(
        l.len() == r.len(),
        "Size of columns must match to apply binary expression"
    );

    let res = match (l.is_const(), r.is_const()) {
        (false, false) => {
            let lhs: &PrimitiveColumn<T> = Series::check_get(&l)?;
            let lhs_chunks_iter = lhs.values().chunks_exact(8);
            let lhs_remainder = lhs_chunks_iter.remainder();

            let rhs: &PrimitiveColumn<T> = Series::check_get(&r)?;
            let rhs_chunks_iter = rhs.values().chunks_exact(8);
            let rhs_remainder = rhs_chunks_iter.remainder();

            let mut values = Vec::with_capacity((lhs.len() + 7) / 8);
            let iterator = lhs_chunks_iter.zip(rhs_chunks_iter).map(|(lhs, rhs)| {
                let lhs = T::Simd::from_chunk(lhs);
                let rhs = T::Simd::from_chunk(rhs);
                op(lhs, rhs)
            });
            values.extend(iterator);

            if !lhs_remainder.is_empty() {
                let lhs = T::Simd::from_incomplete_chunk(lhs_remainder, T::default());
                let rhs = T::Simd::from_incomplete_chunk(rhs_remainder, T::default());
                values.push(op(lhs, rhs))
            };
            MutableBitmap::from_vec(values, lhs.len())
        }
        (false, true) => {
            let lhs: &PrimitiveColumn<T> = Series::check_get(&l)?;
            let lhs_chunks_iter = lhs.values().chunks_exact(8);
            let lhs_remainder = lhs_chunks_iter.remainder();

            let rhs = T::try_create_viewer(&r)?;
            let r = rhs.value_at(0).to_owned_scalar();
            let rhs = T::Simd::from_chunk(&[r; 8]);

            let mut values = Vec::with_capacity((lhs.len() + 7) / 8);
            let iterator = lhs_chunks_iter.map(|lhs| {
                let lhs = T::Simd::from_chunk(lhs);
                op(lhs, rhs)
            });
            values.extend(iterator);

            if !lhs_remainder.is_empty() {
                let lhs = T::Simd::from_incomplete_chunk(lhs_remainder, T::default());
                values.push(op(lhs, rhs))
            };

            MutableBitmap::from_vec(values, lhs.len())
        }
        (true, false) => {
            let lhs = T::try_create_viewer(&l)?;
            let l = lhs.value_at(0).to_owned_scalar();
            let lhs = T::Simd::from_chunk(&[l; 8]);

            let rhs: &PrimitiveColumn<T> = Series::check_get(&r)?;
            let rhs_chunks_iter = rhs.values().chunks_exact(8);
            let rhs_remainder = rhs_chunks_iter.remainder();

            let mut values = Vec::with_capacity((rhs.len() + 7) / 8);
            let iterator = rhs_chunks_iter.map(|rhs| {
                let rhs = T::Simd::from_chunk(rhs);
                op(lhs, rhs)
            });
            values.extend(iterator);

            if !rhs_remainder.is_empty() {
                let rhs = T::Simd::from_incomplete_chunk(rhs_remainder, T::default());
                values.push(op(lhs, rhs))
            };

            MutableBitmap::from_vec(values, rhs.len())
        }
        (true, true) => unreachable!(),
    };
    Ok(BooleanColumn::from_arrow_data(res.into()))
}
