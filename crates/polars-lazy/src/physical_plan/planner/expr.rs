use polars_core::frame::groupby::GroupByMethod;
use polars_core::prelude::*;
use polars_core::series::IsSorted;
use polars_core::utils::_split_offsets;
use polars_core::POOL;
use rayon::prelude::*;

use super::super::expressions as phys_expr;
use crate::prelude::*;

pub(crate) fn create_physical_expressions(
    exprs: &[Node],
    context: Context,
    expr_arena: &Arena<AExpr>,
    schema: Option<&SchemaRef>,
    state: &mut ExpressionConversionState,
) -> PolarsResult<Vec<Arc<dyn PhysicalExpr>>> {
    create_physical_expressions_check_state(exprs, context, expr_arena, schema, state, |_| Ok(()))
}

pub(crate) fn create_physical_expressions_check_state<F>(
    exprs: &[Node],
    context: Context,
    expr_arena: &Arena<AExpr>,
    schema: Option<&SchemaRef>,
    state: &mut ExpressionConversionState,
    checker: F,
) -> PolarsResult<Vec<Arc<dyn PhysicalExpr>>>
where
    F: Fn(&ExpressionConversionState) -> PolarsResult<()>,
{
    exprs
        .iter()
        .map(|e| {
            state.reset();
            let out = create_physical_expr(*e, context, expr_arena, schema, state);
            checker(state)?;
            out
        })
        .collect()
}

#[derive(Copy, Clone, Default)]
pub(crate) struct ExpressionConversionState {
    // settings per context
    // they remain activate between
    // expressions
    has_cache: bool,
    pub allow_threading: bool,
    pub has_windows: bool,
    // settings per expression
    // those are reset every expression
    local: LocalConversionState,
}

#[derive(Copy, Clone, Default)]
struct LocalConversionState {
    has_implode: bool,
    has_window: bool,
}

impl ExpressionConversionState {
    pub(crate) fn new(allow_threading: bool) -> Self {
        Self {
            allow_threading,
            ..Default::default()
        }
    }
    fn reset(&mut self) {
        self.local = Default::default()
    }

    fn has_implode(&self) -> bool {
        self.local.has_implode
    }

    fn set_window(&mut self) {
        self.has_windows = true;
        self.local.has_window = true;
    }
}

pub(crate) fn create_physical_expr(
    expression: Node,
    ctxt: Context,
    expr_arena: &Arena<AExpr>,
    schema: Option<&SchemaRef>,
    state: &mut ExpressionConversionState,
) -> PolarsResult<Arc<dyn PhysicalExpr>> {
    use AExpr::*;

    match expr_arena.get(expression).clone() {
        Count => Ok(Arc::new(phys_expr::CountExpr::new())),
        Window {
            mut function,
            partition_by,
            order_by: _,
            options,
        } => {
            state.set_window();
            // TODO! Order by
            let group_by = create_physical_expressions(
                &partition_by,
                Context::Default,
                expr_arena,
                schema,
                state,
            )?;

            // set again as the state can be reset
            state.set_window();
            let phys_function =
                create_physical_expr(function, Context::Aggregation, expr_arena, schema, state)?;
            let mut out_name = None;
            let mut apply_columns = aexpr_to_leaf_names(function, expr_arena);
            // sort and then dedup removes consecutive duplicates == all duplicates
            apply_columns.sort();
            apply_columns.dedup();

            if apply_columns.is_empty() {
                if has_aexpr(function, expr_arena, |e| matches!(e, AExpr::Literal(_))) {
                    apply_columns.push(Arc::from("literal"))
                } else if has_aexpr(function, expr_arena, |e| matches!(e, AExpr::Count)) {
                    apply_columns.push(Arc::from("count"))
                } else {
                    let e = node_to_expr(function, expr_arena);
                    polars_bail!(
                        ComputeError:
                        "cannot apply a window function, did not find a root column; \
                        this is likely due to a syntax error in this expression: {:?}", e
                    );
                }
            }

            if let Alias(expr, name) = expr_arena.get(function) {
                function = *expr;
                out_name = Some(name.clone());
            };
            let function = node_to_expr(function, expr_arena);

            Ok(Arc::new(WindowExpr {
                group_by,
                apply_columns,
                out_name,
                function,
                phys_function,
                options,
                expr: node_to_expr(expression, expr_arena),
            }))
        }
        Literal(value) => Ok(Arc::new(LiteralExpr::new(
            value,
            node_to_expr(expression, expr_arena),
        ))),
        BinaryExpr { left, op, right } => {
            let lhs = create_physical_expr(left, ctxt, expr_arena, schema, state)?;
            let rhs = create_physical_expr(right, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(phys_expr::BinaryExpr::new(
                lhs,
                op,
                rhs,
                node_to_expr(expression, expr_arena),
            )))
        }
        Column(column) => Ok(Arc::new(ColumnExpr::new(
            column,
            node_to_expr(expression, expr_arena),
            schema.cloned(),
        ))),
        Sort { expr, options } => {
            let phys_expr = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(SortExpr::new(
                phys_expr,
                options,
                node_to_expr(expression, expr_arena),
            )))
        }
        Take { expr, idx } => {
            let phys_expr = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            let phys_idx = create_physical_expr(idx, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(TakeExpr {
                phys_expr,
                idx: phys_idx,
                expr: node_to_expr(expression, expr_arena),
            }))
        }
        SortBy {
            expr,
            by,
            descending,
        } => {
            polars_ensure!(!by.is_empty(), InvalidOperation: "'sort_by' got an empty set");
            let phys_expr = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            let phys_by = create_physical_expressions(&by, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(SortByExpr::new(
                phys_expr,
                phys_by,
                descending,
                node_to_expr(expression, expr_arena),
            )))
        }
        Filter { input, by } => {
            let phys_input = create_physical_expr(input, ctxt, expr_arena, schema, state)?;
            let phys_by = create_physical_expr(by, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(FilterExpr::new(
                phys_input,
                phys_by,
                node_to_expr(expression, expr_arena),
            )))
        }
        Alias(expr, name) => {
            let phys_expr = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(AliasExpr::new(
                phys_expr,
                name,
                node_to_expr(expression, expr_arena),
            )))
        }
        Agg(agg) => {
            let expr = agg.get_input().first();
            let input = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            polars_ensure!(!(state.has_implode() && matches!(ctxt, Context::Aggregation)), InvalidOperation: "'implode' followed by an aggregation is not allowed");
            state.local.has_implode |= matches!(agg, AAggExpr::Implode(_));

            match ctxt {
                // TODO!: implement these functions somewhere else
                // this should not be in the planner.
                Context::Default if !matches!(agg, AAggExpr::Quantile { .. }) => {
                    let function = match agg {
                        AAggExpr::Min { propagate_nans, .. } => {
                            let state = *state;
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = std::mem::take(&mut s[0]);

                                if propagate_nans && s.dtype().is_float() {
                                    #[cfg(feature = "propagate_nans")]
                                    {
                                        return parallel_op_series(
                                            |s| {
                                                Ok(polars_ops::prelude::nan_propagating_aggregate::nan_min_s(&s, s.name()))
                                            },
                                            s,
                                            None,
                                            state,
                                        );
                                    }
                                    #[cfg(not(feature = "propagate_nans"))]
                                    {
                                        panic!("activate 'propagate_nans' feature")
                                    }
                                }

                                match s.is_sorted_flag() {
                                    IsSorted::Ascending | IsSorted::Descending => {
                                        Ok(Some(s.min_as_series()))
                                    }
                                    IsSorted::Not => parallel_op_series(
                                        |s| Ok(s.min_as_series()),
                                        s,
                                        None,
                                        state,
                                    ),
                                }
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::Max { propagate_nans, .. } => {
                            let state = *state;
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = std::mem::take(&mut s[0]);

                                if propagate_nans && s.dtype().is_float() {
                                    #[cfg(feature = "propagate_nans")]
                                    {
                                        return parallel_op_series(
                                            |s| {
                                                Ok(polars_ops::prelude::nan_propagating_aggregate::nan_max_s(&s, s.name()))
                                            },
                                            s,
                                            None,
                                            state,
                                        );
                                    }
                                    #[cfg(not(feature = "propagate_nans"))]
                                    {
                                        panic!("activate 'propagate_nans' feature")
                                    }
                                }

                                match s.is_sorted_flag() {
                                    IsSorted::Ascending | IsSorted::Descending => {
                                        Ok(Some(s.max_as_series()))
                                    }
                                    IsSorted::Not => parallel_op_series(
                                        |s| Ok(s.max_as_series()),
                                        s,
                                        None,
                                        state,
                                    ),
                                }
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::Median(_) => SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                            let s = std::mem::take(&mut s[0]);
                            Ok(Some(s.median_as_series()))
                        })
                            as Arc<dyn SeriesUdf>),
                        AAggExpr::NUnique(_) => {
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = std::mem::take(&mut s[0]);
                                s.n_unique().map(|count| {
                                    Some(
                                        UInt32Chunked::from_slice(s.name(), &[count as u32])
                                            .into_series(),
                                    )
                                })
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::First(_) => SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                            let s = std::mem::take(&mut s[0]);
                            Ok(Some(s.head(Some(1))))
                        })
                            as Arc<dyn SeriesUdf>),
                        AAggExpr::Last(_) => SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                            let s = std::mem::take(&mut s[0]);
                            Ok(Some(s.tail(Some(1))))
                        })
                            as Arc<dyn SeriesUdf>),
                        AAggExpr::Mean(_) => SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                            let s = std::mem::take(&mut s[0]);
                            Ok(Some(s.mean_as_series()))
                        })
                            as Arc<dyn SeriesUdf>),
                        AAggExpr::Implode(_) => {
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = &s[0];
                                s.implode().map(|ca| Some(ca.into_series()))
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::Quantile { .. } => {
                            unreachable!()
                        }
                        AAggExpr::Sum(_) => {
                            let state = *state;
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = std::mem::take(&mut s[0]);
                                parallel_op_series(|s| Ok(s.sum_as_series()), s, None, state)
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::Count(_) => SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                            let s = std::mem::take(&mut s[0]);
                            let count = s.len();
                            Ok(Some(
                                IdxCa::from_slice(s.name(), &[count as IdxSize]).into_series(),
                            ))
                        })
                            as Arc<dyn SeriesUdf>),
                        AAggExpr::Std(_, ddof) => {
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = std::mem::take(&mut s[0]);
                                Ok(Some(s.std_as_series(ddof)))
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::Var(_, ddof) => {
                            SpecialEq::new(Arc::new(move |s: &mut [Series]| {
                                let s = std::mem::take(&mut s[0]);
                                Ok(Some(s.var_as_series(ddof)))
                            }) as Arc<dyn SeriesUdf>)
                        }
                        AAggExpr::AggGroups(_) => {
                            panic!("agg groups expression only supported in aggregation context")
                        }
                    };
                    Ok(Arc::new(ApplyExpr::new_minimal(
                        vec![input],
                        function,
                        node_to_expr(expression, expr_arena),
                        ApplyOptions::ApplyFlat,
                    )))
                }
                _ => {
                    if let AAggExpr::Quantile {
                        expr,
                        quantile,
                        interpol,
                    } = agg
                    {
                        let input = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
                        let quantile =
                            create_physical_expr(quantile, ctxt, expr_arena, schema, state)?;
                        return Ok(Arc::new(AggQuantileExpr::new(input, quantile, interpol)));
                    }
                    let field = schema
                        .map(|schema| {
                            expr_arena.get(expression).to_field(
                                schema,
                                Context::Aggregation,
                                expr_arena,
                            )
                        })
                        .transpose()?;
                    let agg_method: GroupByMethod = agg.into();
                    Ok(Arc::new(AggregationExpr::new(input, agg_method, field)))
                }
            }
        }
        Cast {
            expr,
            data_type,
            strict,
        } => {
            let phys_expr = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(CastExpr {
                input: phys_expr,
                data_type,
                expr: node_to_expr(expression, expr_arena),
                strict,
            }))
        }
        Ternary {
            predicate,
            truthy,
            falsy,
        } => {
            let predicate = create_physical_expr(predicate, ctxt, expr_arena, schema, state)?;
            let truthy = create_physical_expr(truthy, ctxt, expr_arena, schema, state)?;
            let falsy = create_physical_expr(falsy, ctxt, expr_arena, schema, state)?;
            Ok(Arc::new(TernaryExpr::new(
                predicate,
                truthy,
                falsy,
                node_to_expr(expression, expr_arena),
            )))
        }
        AnonymousFunction {
            input,
            function,
            output_type: _,
            options,
        } => {
            let is_reducing_aggregation =
                options.auto_explode && matches!(options.collect_groups, ApplyOptions::ApplyGroups);
            // will be reset in the function so get that here
            let has_window = state.local.has_window;
            let input = create_physical_expressions_check_state(
                &input,
                ctxt,
                expr_arena,
                schema,
                state,
                |state| {
                    polars_ensure!(!((is_reducing_aggregation || has_window) && state.has_implode() && matches!(ctxt, Context::Aggregation)), InvalidOperation: "'implode' followed by an aggregation is not allowed");
                    Ok(())
                },
            )?;

            Ok(Arc::new(ApplyExpr {
                inputs: input,
                function,
                expr: node_to_expr(expression, expr_arena),
                collect_groups: options.collect_groups,
                auto_explode: options.auto_explode,
                allow_rename: options.allow_rename,
                pass_name_to_apply: options.pass_name_to_apply,
                input_schema: schema.cloned(),
                allow_threading: !state.has_cache,
                check_lengths: options.check_lengths(),
                allow_group_aware: options.allow_group_aware,
            }))
        }
        Function {
            input,
            function,
            options,
            ..
        } => {
            let is_reducing_aggregation =
                options.auto_explode && matches!(options.collect_groups, ApplyOptions::ApplyGroups);
            // will be reset in the function so get that here
            let has_window = state.local.has_window;
            let input = create_physical_expressions_check_state(
                &input,
                ctxt,
                expr_arena,
                schema,
                state,
                |state| {
                    polars_ensure!(!((is_reducing_aggregation || has_window) && state.has_implode() && matches!(ctxt, Context::Aggregation)), InvalidOperation: "'implode' followed by an aggregation is not allowed");
                    Ok(())
                },
            )?;

            Ok(Arc::new(ApplyExpr {
                inputs: input,
                function: function.into(),
                expr: node_to_expr(expression, expr_arena),
                collect_groups: options.collect_groups,
                auto_explode: options.auto_explode,
                allow_rename: options.allow_rename,
                pass_name_to_apply: options.pass_name_to_apply,
                input_schema: schema.cloned(),
                allow_threading: !state.has_cache,
                check_lengths: options.check_lengths(),
                allow_group_aware: options.allow_group_aware,
            }))
        }
        Slice {
            input,
            offset,
            length,
        } => {
            let input = create_physical_expr(input, ctxt, expr_arena, schema, state)?;
            let offset = create_physical_expr(offset, ctxt, expr_arena, schema, state)?;
            let length = create_physical_expr(length, ctxt, expr_arena, schema, state)?;
            polars_ensure!(!(state.has_implode() && matches!(ctxt, Context::Aggregation)), InvalidOperation: "'implode' followed by a slice during aggregation is not allowed");
            Ok(Arc::new(SliceExpr {
                input,
                offset,
                length,
                expr: node_to_expr(expression, expr_arena),
            }))
        }
        Explode(expr) => {
            let input = create_physical_expr(expr, ctxt, expr_arena, schema, state)?;
            let function =
                SpecialEq::new(Arc::new(move |s: &mut [Series]| s[0].explode().map(Some))
                    as Arc<dyn SeriesUdf>);
            Ok(Arc::new(ApplyExpr::new_minimal(
                vec![input],
                function,
                node_to_expr(expression, expr_arena),
                ApplyOptions::ApplyGroups,
            )))
        }
        Wildcard => panic!("should be no wildcard at this point"),
        Nth(_) => panic!("should be no nth at this point"),
    }
}

/// Simple wrapper to parallelize functions that can be divided over threads aggregated and
/// finally aggregated in the main thread. This can be done for sum, min, max, etc.
fn parallel_op_series<F>(
    f: F,
    s: Series,
    n_threads: Option<usize>,
    state: ExpressionConversionState,
) -> PolarsResult<Option<Series>>
where
    F: Fn(Series) -> PolarsResult<Series> + Send + Sync,
{
    // set during debug low so
    // we mimic production size data behavior
    #[cfg(debug_assertions)]
    let thread_boundary = 0;

    #[cfg(not(debug_assertions))]
    let thread_boundary = 100_000;

    // threading overhead/ splitting work stealing is costly..
    if !state.allow_threading
        || s.len() < thread_boundary
        || state.has_cache
        || POOL.current_thread_has_pending_tasks().unwrap_or(false)
    {
        return f(s).map(Some);
    }
    let n_threads = n_threads.unwrap_or_else(|| POOL.current_num_threads());
    let splits = _split_offsets(s.len(), n_threads);

    let chunks = POOL.install(|| {
        splits
            .into_par_iter()
            .map(|(offset, len)| {
                let s = s.slice(offset as i64, len);
                f(s)
            })
            .collect::<PolarsResult<Vec<_>>>()
    })?;

    let mut iter = chunks.into_iter();
    let first = iter.next().unwrap();
    let out = iter.fold(first, |mut acc, s| {
        acc.append(&s).unwrap();
        acc
    });

    f(out).map(Some)
}
