// Copyright 2023 RisingWave Labs
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

//! Streaming aggregator implementations that maintain a result value in memory,
//! providing [`StreamingAggImpl`] as their interface.

use std::any::Any;

pub use approx_count_distinct::*;
pub use approx_distinct_append::AppendOnlyStreamingApproxCountDistinct;
use approx_distinct_utils::StreamingApproxCountDistinct;
use dyn_clone::DynClone;
pub use foldable::*;
use risingwave_common::array::stream_chunk::Ops;
use risingwave_common::array::{
    Array, ArrayBuilder, ArrayBuilderImpl, ArrayImpl, BoolArray, BytesArray, DateArray,
    DecimalArray, F32Array, F64Array, I16Array, I32Array, I64Array, Int256Array, IntervalArray,
    JsonbArray, ListArray, StructArray, TimeArray, TimestampArray, TimestamptzArray, Utf8Array,
};
use risingwave_common::buffer::Bitmap;
use risingwave_common::estimate_size::EstimateSize;
use risingwave_common::types::{DataType, Datum};
use risingwave_expr::agg::AggKind;
use risingwave_expr::*;
pub use row_count::*;

use crate::executor::{StreamExecutorError, StreamExecutorResult};

mod approx_count_distinct;
mod approx_distinct_append;
mod approx_distinct_utils;
mod foldable;
mod row_count;

/// `StreamingAggInput` describes the functions needed to feed input data to aggregators.
trait StreamingAggInput<A: Array>: Send + Sync + 'static {
    fn apply_batch_concrete(
        &mut self,
        ops: Ops<'_>,
        visibility: Option<&Bitmap>,
        data: &A,
    ) -> StreamExecutorResult<()>;
}

/// `StreamingAggOutput` allows us to get output from a streaming aggregators.
trait StreamingAggOutput<B: ArrayBuilder>: Send + Sync + 'static {
    fn get_output_concrete(
        &self,
    ) -> StreamExecutorResult<Option<<B::ArrayType as Array>::OwnedItem>>;
}

/// `StreamingAggImpl` erases the associated type information of `StreamingAggInput` and
/// `StreamingAggOutput`. You should manually implement this trait for necessary types.
pub trait StreamingAggImpl:
    Any + std::fmt::Debug + DynClone + EstimateSize + Send + Sync + 'static
{
    /// Apply a batch to the state
    fn apply_batch(
        &mut self,
        ops: Ops<'_>,
        visibility: Option<&Bitmap>,
        data: &[&ArrayImpl],
    ) -> StreamExecutorResult<()>;

    /// Get the output value
    fn get_output(&self) -> StreamExecutorResult<Datum>;

    /// Get the builder of the state output
    fn new_builder(&self) -> ArrayBuilderImpl;

    /// Reset to initial state
    fn reset(&mut self);
}

dyn_clone::clone_trait_object!(StreamingAggImpl);

/// `StreamingSumAgg` sums data of the same type.
type StreamingSumAgg<R, I> =
    StreamingFoldAgg<R, I, PrimitiveSummable<<R as Array>::OwnedItem, <I as Array>::OwnedItem>>;

/// `StreamingSum0Agg` sums data of the same type.
type StreamingSum0Agg = StreamingFoldAgg<I64Array, I64Array, I64Sum0>;

/// `StreamingCountAgg` counts data of any type.
type StreamingCountAgg<S> = StreamingFoldAgg<I64Array, S, Countable<<S as Array>::OwnedItem>>;

/// `StreamingMinAgg` get minimum data of the same type.
type StreamingMinAgg<S> = StreamingFoldAgg<S, S, Minimizable<<S as Array>::OwnedItem>>;

/// `StreamingMaxAgg` get maximum data of the same type.
type StreamingMaxAgg<S> = StreamingFoldAgg<S, S, Maximizable<<S as Array>::OwnedItem>>;

/// `StreamingBitXor`
type StreamingBitXorAgg<S> = StreamingFoldAgg<S, S, BitXorable<<S as Array>::OwnedItem>>;

/// [postgresql specification of aggregate functions](https://www.postgresql.org/docs/13/functions-aggregate.html)
/// Most of the general-purpose aggregate functions have one input except for:
/// 1. `count(*) -> bigint`. The input type of count(*)
/// 2. `json_object_agg ( key "any", value "any" ) -> json`
/// 3. `jsonb_object_agg ( key "any", value "any" ) -> jsonb`
/// 4. `string_agg ( value text, delimiter text ) -> text`
/// 5. `string_agg ( value bytea, delimiter bytea ) -> bytea`
/// We remark that there is difference between `count(*)` and `count(any)`:
/// 1. `count(*)` computes the number of input rows. And the semantics of row count is equal to the
/// semantics of `count(*)` 2. `count("any")` computes the number of input rows in which the input
/// value is not null.
pub fn create_streaming_agg_impl(
    input_types: &[DataType],
    agg_type: &AggKind,
    return_type: &DataType,
    datum: Option<Datum>,
) -> StreamExecutorResult<Box<dyn StreamingAggImpl>> {
    macro_rules! gen_unary_agg_state_match {
        ($agg_type_expr:expr, $input_type_expr:expr, $return_type_expr:expr, $datum: expr,
            [$(($agg_type:ident, $input_type:ident, $return_type:ident, $state_impl:ty)),*$(,)?]) => {
            match (
                $agg_type_expr,
                $input_type_expr,
                $return_type_expr,
                $datum,
            ) {
                $(
                    (AggKind::$agg_type, $input_type! { type_match_pattern }, $return_type! { type_match_pattern }, Some(datum)) => {
                        Box::new(<$state_impl>::with_datum(datum)?)
                    }
                    (AggKind::$agg_type, $input_type! { type_match_pattern }, $return_type! { type_match_pattern }, None) => {
                        Box::new(<$state_impl>::new())
                    }
                )*
                (AggKind::ApproxCountDistinct, _, DataType::Int64, Some(datum)) => {
                    Box::new(UpdatableStreamingApproxCountDistinct::<{approx_count_distinct::DENSE_BITS_DEFAULT}>::with_datum(datum))
                }
                (AggKind::ApproxCountDistinct, _, DataType::Int64, None) => {
                    Box::new(UpdatableStreamingApproxCountDistinct::<{approx_count_distinct::DENSE_BITS_DEFAULT}>::with_no_initial())
                }
                (other_agg, other_input, other_return, _) => panic!(
                    "streaming agg state not implemented: {:?} {:?} {:?}",
                    other_agg, other_input, other_return
                )
            }
        }
    }

    let agg: Box<dyn StreamingAggImpl> = match input_types {
        [input_type] => {
            gen_unary_agg_state_match!(
                agg_type,
                input_type,
                return_type,
                datum,
                [
                    // Count
                    (Count, int64, int64, StreamingCountAgg::<I64Array>),
                    (Count, int32, int64, StreamingCountAgg::<I32Array>),
                    (Count, int16, int64, StreamingCountAgg::<I16Array>),
                    (Count, float64, int64, StreamingCountAgg::<F64Array>),
                    (Count, float32, int64, StreamingCountAgg::<F32Array>),
                    (Count, decimal, int64, StreamingCountAgg::<DecimalArray>),
                    (Count, boolean, int64, StreamingCountAgg::<BoolArray>),
                    (Count, varchar, int64, StreamingCountAgg::<Utf8Array>),
                    (Count, interval, int64, StreamingCountAgg::<IntervalArray>),
                    (Count, date, int64, StreamingCountAgg::<DateArray>),
                    (Count, timestamp, int64, StreamingCountAgg::<TimestampArray>),
                    (
                        Count,
                        timestamptz,
                        int64,
                        StreamingCountAgg::<TimestamptzArray>
                    ),
                    (Count, time, int64, StreamingCountAgg::<TimeArray>),
                    (Count, struct_type, int64, StreamingCountAgg::<StructArray>),
                    (Count, list, int64, StreamingCountAgg::<ListArray>),
                    (Count, bytea, int64, StreamingCountAgg::<BytesArray>),
                    (Count, jsonb, int64, StreamingCountAgg::<JsonbArray>),
                    (Count, int256, int64, StreamingCountAgg::<Int256Array>),
                    // Sum0
                    (Sum0, int64, int64, StreamingSum0Agg),
                    // Sum
                    (Sum, int64, int64, StreamingSumAgg::<I64Array, I64Array>),
                    (
                        Sum,
                        int64,
                        decimal,
                        StreamingSumAgg::<DecimalArray, I64Array>
                    ),
                    (Sum, int32, int64, StreamingSumAgg::<I64Array, I32Array>),
                    (Sum, int16, int64, StreamingSumAgg::<I64Array, I16Array>),
                    (Sum, int32, int32, StreamingSumAgg::<I32Array, I32Array>),
                    (Sum, int16, int16, StreamingSumAgg::<I16Array, I16Array>),
                    (Sum, float32, float64, StreamingSumAgg::<F64Array, F32Array>),
                    (Sum, float32, float32, StreamingSumAgg::<F32Array, F32Array>),
                    (Sum, float64, float64, StreamingSumAgg::<F64Array, F64Array>),
                    (
                        Sum,
                        decimal,
                        decimal,
                        StreamingSumAgg::<DecimalArray, DecimalArray>
                    ),
                    (
                        Sum,
                        interval,
                        interval,
                        StreamingSumAgg::<IntervalArray, IntervalArray>
                    ),
                    (
                        Sum,
                        int256,
                        int256,
                        StreamingSumAgg::<Int256Array, Int256Array>
                    ),
                    // Min
                    (Min, int16, int16, StreamingMinAgg::<I16Array>),
                    (Min, int32, int32, StreamingMinAgg::<I32Array>),
                    (Min, int64, int64, StreamingMinAgg::<I64Array>),
                    (Min, decimal, decimal, StreamingMinAgg::<DecimalArray>),
                    (Min, float32, float32, StreamingMinAgg::<F32Array>),
                    (Min, float64, float64, StreamingMinAgg::<F64Array>),
                    (Min, interval, interval, StreamingMinAgg::<IntervalArray>),
                    (Min, time, time, StreamingMinAgg::<TimeArray>),
                    (Min, date, date, StreamingMinAgg::<DateArray>),
                    (Min, timestamp, timestamp, StreamingMinAgg::<TimestampArray>),
                    (
                        Min,
                        timestamptz,
                        timestamptz,
                        StreamingMinAgg::<TimestamptzArray>
                    ),
                    (Min, varchar, varchar, StreamingMinAgg::<Utf8Array>),
                    (Min, bytea, bytea, StreamingMinAgg::<BytesArray>),
                    (Min, int256, int256, StreamingMinAgg::<Int256Array>),
                    // Max
                    (Max, int16, int16, StreamingMaxAgg::<I16Array>),
                    (Max, int32, int32, StreamingMaxAgg::<I32Array>),
                    (Max, int64, int64, StreamingMaxAgg::<I64Array>),
                    (Max, decimal, decimal, StreamingMaxAgg::<DecimalArray>),
                    (Max, float32, float32, StreamingMaxAgg::<F32Array>),
                    (Max, float64, float64, StreamingMaxAgg::<F64Array>),
                    (Max, interval, interval, StreamingMaxAgg::<IntervalArray>),
                    (Max, time, time, StreamingMaxAgg::<TimeArray>),
                    (Max, date, date, StreamingMaxAgg::<DateArray>),
                    (Max, timestamp, timestamp, StreamingMaxAgg::<TimestampArray>),
                    (
                        Max,
                        timestamptz,
                        timestamptz,
                        StreamingMaxAgg::<TimestamptzArray>
                    ),
                    (Max, varchar, varchar, StreamingMaxAgg::<Utf8Array>),
                    (Max, bytea, bytea, StreamingMaxAgg::<BytesArray>),
                    (Max, int256, int256, StreamingMaxAgg::<Int256Array>),
                    // BitXor
                    (BitXor, int16, int16, StreamingBitXorAgg::<I16Array>),
                    (BitXor, int32, int32, StreamingBitXorAgg::<I32Array>),
                    (BitXor, int64, int64, StreamingBitXorAgg::<I64Array>),
                ]
            )
        }
        [] => {
            match (agg_type, return_type, datum) {
                // According to the function header comments and the link, Count(*) == RowCount
                // `StreamingCountAgg` does not count `NULL`, so we use `StreamingRowCountAgg` here.
                (AggKind::Count, DataType::Int64, Some(datum)) => {
                    Box::new(StreamingRowCountAgg::with_row_cnt(datum))
                }
                (AggKind::Count, DataType::Int64, None) => Box::new(StreamingRowCountAgg::new()),
                _ => {
                    return Err(StreamExecutorError::not_implemented(
                        "unsupported aggregate type",
                        None,
                    ))
                }
            }
        }
        _ => unreachable!(),
    };
    Ok(agg)
}
