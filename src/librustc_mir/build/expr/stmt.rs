// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use build::scope::BreakableScope;
use build::{BlockAnd, BlockAndExtension, BlockFrame, Builder};
use hair::*;
use rustc::mir::*;

impl<'a, 'gcx, 'tcx> Builder<'a, 'gcx, 'tcx> {
    /// Builds a block of MIR statements to evaluate the HAIR `expr`.
    /// If the original expression was an AST statement,
    /// (e.g. `some().code(&here());`) then `opt_stmt_span` is the
    /// span of that statement (including its semicolon, if any).
    /// Diagnostics use this span (which may be larger than that of
    /// `expr`) to identify when statement temporaries are dropped.
    pub fn stmt_expr(&mut self,
                     mut block: BasicBlock,
                     expr: Expr<'tcx>,
                     opt_stmt_span: Option<StatementSpan>)
                     -> BlockAnd<()>
    {
        let this = self;
        let expr_span = expr.span;
        let source_info = this.source_info(expr.span);
        // Handle a number of expressions that don't need a destination at all. This
        // avoids needing a mountain of temporary `()` variables.
        let expr2 = expr.clone();
        match expr.kind {
            ExprKind::Scope {
                region_scope,
                lint_level,
                value,
            } => {
                let value = this.hir.mirror(value);
                this.in_scope((region_scope, source_info), lint_level, block, |this| {
                    this.stmt_expr(block, value, opt_stmt_span)
                })
            }
            ExprKind::Assign { lhs, rhs } => {
                let lhs = this.hir.mirror(lhs);
                let rhs = this.hir.mirror(rhs);
                let lhs_span = lhs.span;

                // Note: we evaluate assignments right-to-left. This
                // is better for borrowck interaction with overloaded
                // operators like x[j] = x[i].

                debug!("stmt_expr Assign block_context.push(SubExpr) : {:?}", expr2);
                this.block_context.push(BlockFrame::SubExpr);

                // Generate better code for things that don't need to be
                // dropped.
                if this.hir.needs_drop(lhs.ty) {
                    let rhs = unpack!(block = this.as_local_operand(block, rhs));
                    let lhs = unpack!(block = this.as_place(block, lhs));
                    unpack!(block = this.build_drop_and_replace(block, lhs_span, lhs, rhs));
                } else {
                    let rhs = unpack!(block = this.as_local_rvalue(block, rhs));
                    let lhs = unpack!(block = this.as_place(block, lhs));
                    this.cfg.push_assign(block, source_info, &lhs, rhs);
                }

                this.block_context.pop();
                block.unit()
            }
            ExprKind::AssignOp { op, lhs, rhs } => {
                // FIXME(#28160) there is an interesting semantics
                // question raised here -- should we "freeze" the
                // value of the lhs here?  I'm inclined to think not,
                // since it seems closer to the semantics of the
                // overloaded version, which takes `&mut self`.  This
                // only affects weird things like `x += {x += 1; x}`
                // -- is that equal to `x + (x + 1)` or `2*(x+1)`?

                let lhs = this.hir.mirror(lhs);
                let lhs_ty = lhs.ty;

                debug!("stmt_expr AssignOp block_context.push(SubExpr) : {:?}", expr2);
                this.block_context.push(BlockFrame::SubExpr);

                // As above, RTL.
                let rhs = unpack!(block = this.as_local_operand(block, rhs));
                let lhs = unpack!(block = this.as_place(block, lhs));

                // we don't have to drop prior contents or anything
                // because AssignOp is only legal for Copy types
                // (overloaded ops should be desugared into a call).
                let result = unpack!(
                    block = this.build_binary_op(
                        block,
                        op,
                        expr_span,
                        lhs_ty,
                        Operand::Copy(lhs.clone()),
                        rhs
                    )
                );
                this.cfg.push_assign(block, source_info, &lhs, result);

                this.block_context.pop();
                block.unit()
            }
            ExprKind::Continue { label } => {
                let BreakableScope {
                    continue_block,
                    region_scope,
                    ..
                } = *this.find_breakable_scope(expr_span, label);
                let continue_block = continue_block
                    .expect("Attempted to continue in non-continuable breakable block");
                this.exit_scope(
                    expr_span,
                    (region_scope, source_info),
                    block,
                    continue_block,
                );
                this.cfg.start_new_block().unit()
            }
            ExprKind::Break { label, value } => {
                let (break_block, region_scope, destination) = {
                    let BreakableScope {
                        break_block,
                        region_scope,
                        ref break_destination,
                        ..
                    } = *this.find_breakable_scope(expr_span, label);
                    (break_block, region_scope, break_destination.clone())
                };
                if let Some(value) = value {
                    debug!("stmt_expr Break val block_context.push(SubExpr) : {:?}", expr2);
                    this.block_context.push(BlockFrame::SubExpr);
                    unpack!(block = this.into(&destination, block, value));
                    this.block_context.pop();
                } else {
                    this.cfg.push_assign_unit(block, source_info, &destination)
                }
                this.exit_scope(expr_span, (region_scope, source_info), block, break_block);
                this.cfg.start_new_block().unit()
            }
            ExprKind::Return { value } => {
                block = match value {
                    Some(value) => {
                        debug!("stmt_expr Return val block_context.push(SubExpr) : {:?}", expr2);
                        this.block_context.push(BlockFrame::SubExpr);
                        let result = unpack!(this.into(&Place::Local(RETURN_PLACE), block, value));
                        this.block_context.pop();
                        result
                    }
                    None => {
                        this.cfg
                            .push_assign_unit(block, source_info, &Place::Local(RETURN_PLACE));
                        block
                    }
                };
                let region_scope = this.region_scope_of_return_scope();
                let return_block = this.return_block();
                this.exit_scope(expr_span, (region_scope, source_info), block, return_block);
                this.cfg.start_new_block().unit()
            }
            ExprKind::InlineAsm {
                asm,
                outputs,
                inputs,
            } => {
                debug!("stmt_expr InlineAsm block_context.push(SubExpr) : {:?}", expr2);
                this.block_context.push(BlockFrame::SubExpr);
                let outputs = outputs
                    .into_iter()
                    .map(|output| unpack!(block = this.as_place(block, output)))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                let inputs = inputs
                    .into_iter()
                    .map(|input| {
                        (
                            input.span(),
                            unpack!(block = this.as_local_operand(block, input)),
                        )
                    }).collect::<Vec<_>>()
                    .into_boxed_slice();
                this.cfg.push(
                    block,
                    Statement {
                        source_info,
                        kind: StatementKind::InlineAsm {
                            asm: box asm.clone(),
                            outputs,
                            inputs,
                        },
                    },
                );
                this.block_context.pop();
                block.unit()
            }
            _ => {
                let expr_ty = expr.ty;
                let temp = this.temp(expr.ty.clone(), expr_span);
                unpack!(block = this.into(&temp, block, expr));

                // Attribute drops of the statement's temps to the
                // semicolon at the statement's end.
                let drop_point = this.hir.tcx().sess.source_map().end_point(match opt_stmt_span {
                    None => expr_span,
                    Some(StatementSpan(span)) => span,
                });

                unpack!(block = this.build_drop(block, drop_point, temp, expr_ty));
                block.unit()
            }
        }
    }
}
