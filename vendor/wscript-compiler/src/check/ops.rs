//! Operator checking: binary, unary and compound assignment.
//!
//! One ladder, written once. `arith_result`, `check_unary`'s `Neg` arm,
//! `check_eq` and `check_cmp` were four copies of the same eight-way
//! dispatch over the resolved operand type — `Int`, `Float`, `Str`, `Var`,
//! quantity, `Named`, `Param`, `Error | Never`, other — with the policy of
//! each arm differing per operator. Unit families and generics both had to
//! edit every copy, which is why `check/expr.rs` grew by 421 and 211 lines
//! for those two features.
//!
//! The module is split at an internal seam:
//!
//! - [`lower`] and [`expect_rhs`] are **pure**: they see an [`Operand`]
//!   descriptor, not the checker, and decide the lowering, the result type
//!   and the diagnostic. They are unit-tested directly.
//! - The `Checker` methods at the bottom are the effectful shell: they
//!   check operands in the order the table asks for, build descriptors,
//!   and record or emit whatever the pure layer decided.
//!
//! Message text lives with the table, in [`Msg`], rendered on the error
//! path only so nothing allocates a type name for an operation that
//! succeeds.

use wscript_core::defs::{self, DefId};
use wscript_core::span::Span;
use wscript_core::types::Type;

use super::{BinOpKind, BoundKind, Checker, Lowering, PrimKind, UnOpKind};
use crate::ast::{BinOp, Expr, NodeId};

// ------------------------------------------------------------------ ops

/// An operator, as the table sees it. `&&`/`||` are not here: they are
/// bool-only, have no ladder, and stay in `check_expr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Op {
    /// `+ - * / %`
    Arith(BinOp),
    /// Unary `-`
    Neg,
    /// `==` / `!=`
    Eq { negate: bool },
    /// `< <= > >=`
    Cmp(BinOp),
}

impl Op {
    fn symbol(self) -> &'static str {
        match self {
            Op::Arith(op) | Op::Cmp(op) => op_symbol(op),
            Op::Neg => "-",
            Op::Eq { negate: false } => "==",
            Op::Eq { negate: true } => "!=",
        }
    }

    /// The operator trait a `Named` operand must implement, if any.
    fn trait_id(self) -> Option<DefId> {
        Some(match self {
            Op::Arith(BinOp::Add) => defs::TRAIT_ADD,
            Op::Arith(BinOp::Sub) => defs::TRAIT_SUB,
            Op::Arith(BinOp::Mul) => defs::TRAIT_MUL,
            Op::Arith(BinOp::Div) => defs::TRAIT_DIV,
            Op::Arith(_) => defs::TRAIT_REM,
            Op::Neg => defs::TRAIT_NEG,
            // Eq/Ord are consulted through `impl_maps`, not `trait_impls`.
            Op::Eq { .. } | Op::Cmp(_) => return None,
        })
    }

    /// The bound a `Param` operand needs. Arithmetic on type parameters is
    /// not supported in v1, so it has none and always errors.
    fn bound(self) -> Option<BoundKind> {
        match self {
            Op::Eq { .. } => Some(BoundKind::Eq),
            Op::Cmp(_) => Some(BoundKind::Ord),
            Op::Arith(_) | Op::Neg => None,
        }
    }

    /// The diagnostic code this operator reports under.
    fn code(self) -> &'static str {
        match self {
            Op::Arith(_) | Op::Neg => "E0234",
            Op::Eq { .. } | Op::Cmp(_) => "E0235",
        }
    }

    fn is_relational(self) -> bool {
        matches!(self, Op::Eq { .. } | Op::Cmp(_))
    }
}

// -------------------------------------------------------------- operand

/// What an operand looks like to the ladder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Shape {
    Int,
    Float,
    Str,
    Bool,
    Char,
    Unit,
    /// An unresolved inference variable.
    Var,
    /// A unit family: which one, and whether it is float-backed. The
    /// family id is what distinguishes `D / D` (a ratio) from `D / D'`
    /// (a derived dimension, which v1 does not model).
    Quantity {
        def: DefId,
        float_base: bool,
    },
    /// A nominal type (struct/enum).
    Named,
    /// A rigid type parameter.
    Param,
    /// `Option`/`Result`/`List`/`Map` — structurally comparable if their
    /// elements are.
    Container,
    /// `Error` or `Never`: already reported, do not report again.
    Poison,
    /// Anything else (`fn`, `weak`, `dyn`).
    Other,
}

/// An operand, reduced to the facts the ladder needs.
///
/// Three fields cover all four ladders because the shell resolves the
/// operator-specific question before building the descriptor: `proto` is
/// the `Add` impl for `+` and the `Eq` impl for `==`, and `structural`
/// means "derives it" for a struct, "element types support it" for a
/// container, and "declares the bound" for a type parameter.
#[derive(Clone, Debug)]
pub(crate) struct Operand {
    pub shape: Shape,
    /// A user `impl` of this operator's trait, if any.
    pub proto: Option<u32>,
    /// Supports the operation without a user impl.
    pub structural: bool,
}

impl Operand {
    #[cfg(test)]
    fn of(shape: Shape) -> Operand {
        Operand {
            shape,
            proto: None,
            structural: false,
        }
    }
}

// --------------------------------------------------------------- result

/// What the expression evaluates to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ResultTy {
    /// The left operand's own (resolved) type.
    Operand,
    /// The *right* operand's type — `n * D` produces the quantity, not
    /// the number.
    Rhs,
    /// A quantity's backing number — `D / D` and unary conversions.
    Base,
    Bool,
}

/// The ladder's verdict.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    Lower(Lowering, ResultTy),
    /// An unconstrained operand: unify it with `int` first, then lower.
    /// Poisons instead if that unification fails.
    DefaultInt(Lowering, ResultTy),
    Err {
        code: &'static str,
        msg: Msg,
    },
    /// Already-reported error; produce `Type::Error` silently.
    Poison,
}

/// Which operand a message names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    Lhs,
    Rhs,
}

/// A diagnostic, named by shape rather than by text, so the message and
/// its help live beside the table that produces them and nothing
/// allocates a type name unless an error is actually reported.
#[derive(Clone, Debug)]
pub(crate) enum Msg {
    /// No such operator for this type.
    NoOperator { op: Op, slot: Slot },
    /// A `Named` type needs a user impl of the operator's trait.
    NeedsTrait { op: Op, slot: Slot },
    /// `==` on a struct/enum needs `Eq`.
    NeedsEq { slot: Slot },
    /// `<` and friends need `Ord`.
    NeedsOrd { slot: Slot },
    /// A container's element type does not support the operation.
    ElementsUnsupported { op: Op, slot: Slot },
    /// A type parameter lacks the required bound.
    NeedsBound { op: Op, slot: Slot },
    /// Arithmetic on a type parameter — not supported in v1 at all.
    NoArithOnParam { op: Op, slot: Slot },
    /// `unit` has one value, so comparing it is meaningless.
    CannotCompareUnit,
    /// Function/weak/dyn support reference identity only.
    IdentityOnly { op: Op, slot: Slot },
    /// A number may only scale a quantity, not add to or divide it.
    ScaleOnly {
        op: Op,
        number: Slot,
        quantity: Slot,
    },
    /// Two quantities cannot be multiplied (no derived dimensions).
    NoDerivedDimension { a: Slot, b: Slot },
    /// Division across two different families.
    CrossFamilyDivide { a: Slot, b: Slot },
}

/// The type names a message needs, resolved once on the error path.
pub(crate) struct Names {
    pub lhs: String,
    pub rhs: String,
}

impl Names {
    fn get(&self, slot: Slot) -> &str {
        match slot {
            Slot::Lhs => &self.lhs,
            Slot::Rhs => &self.rhs,
        }
    }
}

impl Msg {
    /// `(message, help)`.
    pub(crate) fn render(&self, n: &Names) -> (String, String) {
        match self {
            Msg::NoOperator { op, slot } => {
                let ty = n.get(*slot);
                let help = if *op == Op::Arith(BinOp::Add) || matches!(op, Op::Arith(_)) {
                    if ty == "string" {
                        "strings support `+` for concatenation only".to_string()
                    } else {
                        "arithmetic operators work on int and float \
                         (and types implementing the operator traits)"
                            .to_string()
                    }
                } else {
                    "this operator is not defined for that type".to_string()
                };
                (format!("no `{}` operator for `{ty}`", op.symbol()), help)
            }
            Msg::NeedsTrait { op, slot } => {
                let ty = n.get(*slot);
                let tr = trait_name(*op);
                (
                    if *op == Op::Neg {
                        format!("cannot negate `{ty}`")
                    } else {
                        format!("no `{}` operator for `{ty}`", op.symbol())
                    },
                    format!("implement the `{tr}` trait: `impl {tr} for {ty}`"),
                )
            }
            Msg::NeedsEq { slot } => {
                let ty = n.get(*slot);
                (
                    format!("`==` on `{ty}` requires an `Eq` implementation"),
                    format!(
                        "add `#[derive(Eq)]` to `{ty}`, or `impl Eq for {ty}`; \
                         for reference identity use `same(a, b)` (PRD §3.7)"
                    ),
                )
            }
            Msg::NeedsOrd { slot } => {
                let ty = n.get(*slot);
                (
                    format!("ordering comparison on `{ty}` requires `Ord`"),
                    format!("add `#[derive(Eq, Ord)]` to `{ty}`, or `impl Ord for {ty}`"),
                )
            }
            Msg::ElementsUnsupported { op, slot } => (
                format!(
                    "`{}` on `{}` requires the element type to support `{}`",
                    op.symbol(),
                    n.get(*slot),
                    op.symbol()
                ),
                "element types must be primitives, strings, or Eq types".to_string(),
            ),
            Msg::NeedsBound { op, slot } => {
                let pn = n.get(*slot);
                let bound = match op {
                    Op::Cmp(_) => "Ord",
                    _ => "Eq",
                };
                (
                    format!(
                        "{} on `{pn}` requires an `{bound}` bound",
                        if matches!(op, Op::Cmp(_)) {
                            "ordering comparison".to_string()
                        } else {
                            format!("`{}`", op.symbol())
                        }
                    ),
                    format!("declare the parameter with a bound: `[{pn}: {bound}]`"),
                )
            }
            Msg::NoArithOnParam { op, slot } => (
                format!(
                    "no `{}` operator for the type parameter `{}`",
                    op.symbol(),
                    n.get(*slot)
                ),
                "arithmetic bounds on type parameters arrive in a later release; \
                 take concrete numeric types for now"
                    .to_string(),
            ),
            Msg::CannotCompareUnit => (
                "cannot compare unit values".to_string(),
                "`unit` has only one value; the comparison is always true".to_string(),
            ),
            Msg::IdentityOnly { op, slot } => (
                format!("`{}` is not supported for `{}`", op.symbol(), n.get(*slot)),
                "function, weak and dyn values support `same(a, b)` reference identity only"
                    .to_string(),
            ),
            Msg::ScaleOnly {
                op,
                number,
                quantity,
            } => {
                let (num, q) = (n.get(*number), n.get(*quantity));
                (
                    format!("no `{}` operator for `{num}` and `{q}`", op.symbol()),
                    format!("a number can only scale a unit value: `{q} * n` or `n * {q}`"),
                )
            }
            Msg::NoDerivedDimension { a, b } => (
                format!("cannot multiply `{}` by `{}`", n.get(*a), n.get(*b)),
                "multiplying two unit values would produce a new dimension, \
                 which this release does not model — scale by a plain number instead"
                    .to_string(),
            ),
            Msg::CrossFamilyDivide { a, b } => (
                format!("cannot divide `{}` by `{}`", n.get(*a), n.get(*b)),
                "dividing across unit families would produce a new dimension, \
                 which this release does not model"
                    .to_string(),
            ),
        }
    }
}

fn trait_name(op: Op) -> &'static str {
    match op {
        Op::Arith(BinOp::Add) => "Add",
        Op::Arith(BinOp::Sub) => "Sub",
        Op::Arith(BinOp::Mul) => "Mul",
        Op::Arith(BinOp::Div) => "Div",
        Op::Arith(_) => "Rem",
        Op::Neg => "Neg",
        Op::Eq { .. } => "Eq",
        Op::Cmp(_) => "Ord",
    }
}

pub(crate) fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

// ------------------------------------------------------------ pure core

/// What the right operand should be checked against, given the left.
///
/// This lives in the table rather than the shell because it is operator
/// knowledge: `+` on a quantity wants another quantity, `*` wants the
/// backing number, `/` wants neither, and comparisons want the left type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Expect {
    /// No expectation — infer freely.
    Free,
    /// The left operand's own type.
    SameAsLhs,
    /// A quantity's backing number.
    Base,
}

/// What the shell must unify once the right operand is checked.
///
/// Getting this from the table rather than from a shape test is what keeps
/// `D + D'` (mixed families — an error) distinct from `D * n` (scaling —
/// legitimate). Both have a quantity on the left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Unify {
    /// `unify(lhs, rhs)` with the caller's own message.
    SameType,
    /// `unify(lhs, rhs)`, explaining it this way.
    SameTypeBecause(&'static str),
    /// The family's backing type must match the plain number on `number`.
    /// Which side that is matters: `n * D` checks the *left* operand
    /// against the right's family, `D * n` the other way round.
    Backing { number: Slot, why: &'static str },
    /// Nothing — the ladder resolves the pairing itself.
    Nothing,
}

const SAME_FAMILY: &str = "both operands must be values of the same unit family";
const SCALE_NEEDS_BACKING: &str = "scaling a unit value requires its backing type";
const DIVIDE_NEEDS_BACKING: &str =
    "dividing a unit value requires its own family or its backing type";

/// How to check and pair the right operand, given the operator and the
/// left one.
pub(crate) fn rhs_rule(op: Op, lhs: &Operand) -> (Expect, Unify) {
    match (op, lhs.shape) {
        // Scaling and same-family combination, per PRD §3.10:
        //   D + D   D - D   D % D        D * n        D / n | D / D
        (Op::Arith(BinOp::Mul), Shape::Quantity { .. }) => (
            Expect::Base,
            Unify::Backing {
                number: Slot::Rhs,
                why: SCALE_NEEDS_BACKING,
            },
        ),
        (Op::Arith(BinOp::Div), Shape::Quantity { .. }) => (Expect::Free, Unify::Nothing),
        (Op::Arith(_), Shape::Quantity { .. }) => {
            (Expect::SameAsLhs, Unify::SameTypeBecause(SAME_FAMILY))
        }
        // `n * D` is only discoverable by checking the right side freely;
        // the ladder then decides whether it is a scale or an error.
        (Op::Arith(_), Shape::Int | Shape::Float) => (Expect::Free, Unify::Nothing),
        (Op::Arith(_), _) => (Expect::SameAsLhs, Unify::SameType),
        (Op::Eq { .. } | Op::Cmp(_), _) => (Expect::SameAsLhs, Unify::SameType),
        (Op::Neg, _) => (Expect::Free, Unify::Nothing),
    }
}

/// After `n * D` or `n / D`, the number must match the family's backing
/// type. `None` when no such pairing applies.
pub(crate) fn post_unify(op: Op, lhs: &Operand, rhs: &Operand) -> Option<Unify> {
    let l_num = matches!(lhs.shape, Shape::Int | Shape::Float);
    let r_qty = matches!(rhs.shape, Shape::Quantity { .. });
    match (op, l_num, r_qty) {
        (Op::Arith(BinOp::Mul), true, true) => Some(Unify::Backing {
            number: Slot::Lhs,
            why: SCALE_NEEDS_BACKING,
        }),
        // A plain number on the left had to be checked freely so `n * D`
        // was discoverable at all; once the right side turns out not to be
        // a quantity, ordinary same-type arithmetic applies. The original
        // reached this by checking the right operand a second time.
        (Op::Arith(_), true, false) => Some(Unify::SameType),
        // `D * D` is rejected by the ladder as a derived dimension.
        // Unifying as well would add a spurious E0220 behind it.
        (Op::Arith(BinOp::Mul), false, true) if matches!(lhs.shape, Shape::Quantity { .. }) => {
            Some(Unify::Nothing)
        }
        // `D / n` — the divisor must be the backing number.
        (Op::Arith(BinOp::Div), false, false) if matches!(lhs.shape, Shape::Quantity { .. }) => {
            Some(Unify::Backing {
                number: Slot::Rhs,
                why: DIVIDE_NEEDS_BACKING,
            })
        }
        _ => None,
    }
}

/// Resolve `op` over its operands.
///
/// `rhs` is `None` for unary operators and for compound assignment, where
/// the two sides have already been unified.
pub(crate) fn lower(op: Op, lhs: &Operand, rhs: Option<&Operand>) -> Outcome {
    // Deliberately no short-circuit on a poisoned *right* operand: the
    // shell's unify absorbs it, and the ladder then classifies the left
    // type as it would have anyway. Poisoning here instead would turn
    // `int + <error>` from `int` into `error` and suppress later checks.
    // Each ladder handles a poisoned *left* operand itself, because they
    // disagree: arithmetic and ordering propagate, `==` accepts.

    // Mixed quantity/number arithmetic is the only case where the two
    // sides legitimately differ, so it is resolved before the ladder.
    if let Op::Arith(bin) = op
        && let Some(rhs) = rhs
        && let Some(outcome) = mixed_quantity(bin, lhs, rhs)
    {
        return outcome;
    }

    match op {
        Op::Arith(bin) => arith(op, bin, lhs),
        Op::Neg => neg(op, lhs),
        Op::Eq { negate } => eq(op, negate, lhs),
        Op::Cmp(bin) => cmp(op, bin, lhs),
    }
}

/// `n * D`, `D * n`, `D / n`, `D / D`, and the errors around them.
/// `None` when neither side is a quantity, or both are the same family and
/// the ordinary ladder applies.
fn mixed_quantity(bin: BinOp, lhs: &Operand, rhs: &Operand) -> Option<Outcome> {
    let l_qty = matches!(lhs.shape, Shape::Quantity { .. });
    let r_qty = matches!(rhs.shape, Shape::Quantity { .. });
    let l_num = matches!(lhs.shape, Shape::Int | Shape::Float);

    match (l_qty, r_qty) {
        // `n * D` — a plain number scaling a quantity.
        (false, true) if l_num => Some(if bin == BinOp::Mul {
            let Shape::Quantity { float_base, .. } = rhs.shape else {
                unreachable!()
            };
            Outcome::Lower(arith_prim(bin, float_base), ResultTy::Rhs)
        } else {
            Outcome::Err {
                code: "E0234",
                msg: Msg::ScaleOnly {
                    op: Op::Arith(bin),
                    number: Slot::Lhs,
                    quantity: Slot::Rhs,
                },
            }
        }),
        (true, true) => {
            let (
                Shape::Quantity {
                    def: l_def,
                    float_base,
                },
                Shape::Quantity { def: r_def, .. },
            ) = (lhs.shape, rhs.shape)
            else {
                unreachable!()
            };
            match bin {
                // Two quantities multiplied would make a new dimension.
                BinOp::Mul => Some(Outcome::Err {
                    code: "E0234",
                    msg: Msg::NoDerivedDimension {
                        a: Slot::Lhs,
                        b: Slot::Rhs,
                    },
                }),
                // `D / D` is a plain ratio in the backing number — but only
                // within one family. Across families it would be a derived
                // dimension, and `/` is the one operator the shell does not
                // unify first, so this is where that is caught.
                BinOp::Div if l_def == r_def => {
                    Some(Outcome::Lower(arith_prim(bin, float_base), ResultTy::Base))
                }
                BinOp::Div => Some(Outcome::Err {
                    code: "E0234",
                    msg: Msg::CrossFamilyDivide {
                        a: Slot::Lhs,
                        b: Slot::Rhs,
                    },
                }),
                _ => None,
            }
        }
        // `D * n` / `D / n` — scaling down. Same-family `+ - %` falls
        // through to the ladder.
        (true, false) => {
            let Shape::Quantity { float_base, .. } = lhs.shape else {
                unreachable!()
            };
            match bin {
                BinOp::Mul | BinOp::Div => Some(Outcome::Lower(
                    arith_prim(bin, float_base),
                    ResultTy::Operand,
                )),
                _ => None,
            }
        }
        _ => None,
    }
}

fn arith_prim(bin: BinOp, float: bool) -> Lowering {
    Lowering::BinOp(if float {
        BinOpKind::FloatArith(bin)
    } else {
        BinOpKind::IntArith(bin)
    })
}

fn arith(op: Op, bin: BinOp, a: &Operand) -> Outcome {
    match a.shape {
        Shape::Int => Outcome::Lower(arith_prim(bin, false), ResultTy::Operand),
        Shape::Float => Outcome::Lower(arith_prim(bin, true), ResultTy::Operand),
        Shape::Str if bin == BinOp::Add => {
            Outcome::Lower(Lowering::BinOp(BinOpKind::Concat), ResultTy::Operand)
        }
        // Unconstrained operands (a closure parameter used only here)
        // default to int.
        Shape::Var => Outcome::DefaultInt(arith_prim(bin, false), ResultTy::Operand),
        // Same-family `D + D`, `D - D`, `D % D` — plain numbers at runtime.
        Shape::Quantity { float_base, .. } => {
            Outcome::Lower(arith_prim(bin, float_base), ResultTy::Operand)
        }
        Shape::Named => match a.proto {
            Some(proto) => Outcome::Lower(
                Lowering::BinOp(BinOpKind::ArithCall { proto }),
                ResultTy::Operand,
            ),
            None => Outcome::Err {
                code: op.code(),
                msg: Msg::NeedsTrait {
                    op,
                    slot: Slot::Lhs,
                },
            },
        },
        Shape::Param => Outcome::Err {
            code: "E0253",
            msg: Msg::NoArithOnParam {
                op,
                slot: Slot::Lhs,
            },
        },
        Shape::Poison => Outcome::Poison,
        _ => Outcome::Err {
            code: op.code(),
            msg: Msg::NoOperator {
                op,
                slot: Slot::Lhs,
            },
        },
    }
}

fn neg(op: Op, a: &Operand) -> Outcome {
    match a.shape {
        Shape::Int => Outcome::Lower(Lowering::UnOp(UnOpKind::NegInt), ResultTy::Operand),
        Shape::Float => Outcome::Lower(Lowering::UnOp(UnOpKind::NegFloat), ResultTy::Operand),
        Shape::Var => Outcome::DefaultInt(Lowering::UnOp(UnOpKind::NegInt), ResultTy::Operand),
        // A unit value negates as the number it is stored in.
        Shape::Quantity { float_base, .. } => Outcome::Lower(
            Lowering::UnOp(if float_base {
                UnOpKind::NegFloat
            } else {
                UnOpKind::NegInt
            }),
            ResultTy::Operand,
        ),
        Shape::Named => match a.proto {
            Some(proto) => Outcome::Lower(
                Lowering::UnOp(UnOpKind::NegCall { proto }),
                ResultTy::Operand,
            ),
            None => Outcome::Err {
                code: op.code(),
                msg: Msg::NeedsTrait {
                    op,
                    slot: Slot::Lhs,
                },
            },
        },
        Shape::Poison => Outcome::Poison,
        _ => Outcome::Err {
            code: op.code(),
            msg: Msg::NeedsTrait {
                op,
                slot: Slot::Lhs,
            },
        },
    }
}

fn eq(op: Op, negate: bool, a: &Operand) -> Outcome {
    let value = Lowering::BinOp(BinOpKind::EqValue { negate });
    let prim = |k: PrimKind| Lowering::BinOp(BinOpKind::EqPrim { kind: k, negate });
    match a.shape {
        Shape::Int => Outcome::Lower(prim(PrimKind::Int), ResultTy::Bool),
        Shape::Float => Outcome::Lower(prim(PrimKind::Float), ResultTy::Bool),
        Shape::Bool => Outcome::Lower(prim(PrimKind::Bool), ResultTy::Bool),
        Shape::Char => Outcome::Lower(prim(PrimKind::Char), ResultTy::Bool),
        Shape::Str => Outcome::Lower(prim(PrimKind::Str), ResultTy::Bool),
        // Quantities compare as the number they are stored in.
        Shape::Quantity { float_base, .. } => Outcome::Lower(
            prim(if float_base {
                PrimKind::Float
            } else {
                PrimKind::Int
            }),
            ResultTy::Bool,
        ),
        // Unconstrained: accept and lower structurally. Unlike arithmetic,
        // `==` does not default to int — any type may be compared.
        Shape::Var => Outcome::Lower(value, ResultTy::Bool),
        Shape::Named => match (a.proto, a.structural) {
            (Some(proto), _) => Outcome::Lower(
                Lowering::BinOp(BinOpKind::EqCall { proto, negate }),
                ResultTy::Bool,
            ),
            (None, true) => Outcome::Lower(value, ResultTy::Bool),
            (None, false) => Outcome::Err {
                code: op.code(),
                msg: Msg::NeedsEq { slot: Slot::Lhs },
            },
        },
        Shape::Container if a.structural => Outcome::Lower(value, ResultTy::Bool),
        Shape::Container => Outcome::Err {
            code: op.code(),
            msg: Msg::ElementsUnsupported {
                op,
                slot: Slot::Lhs,
            },
        },
        Shape::Param if a.structural => Outcome::Lower(value, ResultTy::Bool),
        Shape::Param => Outcome::Err {
            code: "E0253",
            msg: Msg::NeedsBound {
                op,
                slot: Slot::Lhs,
            },
        },
        Shape::Unit => Outcome::Err {
            code: op.code(),
            msg: Msg::CannotCompareUnit,
        },
        Shape::Poison => Outcome::Lower(value, ResultTy::Bool),
        Shape::Other => Outcome::Err {
            code: op.code(),
            msg: Msg::IdentityOnly {
                op,
                slot: Slot::Lhs,
            },
        },
    }
}

fn cmp(op: Op, bin: BinOp, a: &Operand) -> Outcome {
    let prim = |k: PrimKind| Lowering::BinOp(BinOpKind::CmpPrim { kind: k, op: bin });
    match a.shape {
        Shape::Int => Outcome::Lower(prim(PrimKind::Int), ResultTy::Bool),
        Shape::Float => Outcome::Lower(prim(PrimKind::Float), ResultTy::Bool),
        Shape::Char => Outcome::Lower(prim(PrimKind::Char), ResultTy::Bool),
        Shape::Str => Outcome::Lower(prim(PrimKind::Str), ResultTy::Bool),
        Shape::Quantity { float_base, .. } => Outcome::Lower(
            prim(if float_base {
                PrimKind::Float
            } else {
                PrimKind::Int
            }),
            ResultTy::Bool,
        ),
        Shape::Var => Outcome::DefaultInt(prim(PrimKind::Int), ResultTy::Bool),
        Shape::Named => match (a.proto, a.structural) {
            (Some(proto), _) => Outcome::Lower(
                Lowering::BinOp(BinOpKind::CmpCall { proto, op: bin }),
                ResultTy::Bool,
            ),
            (None, true) => Outcome::Lower(
                Lowering::BinOp(BinOpKind::CmpValue { op: bin }),
                ResultTy::Bool,
            ),
            (None, false) => Outcome::Err {
                code: op.code(),
                msg: Msg::NeedsOrd { slot: Slot::Lhs },
            },
        },
        Shape::Param if a.structural => Outcome::Lower(
            Lowering::BinOp(BinOpKind::CmpValue { op: bin }),
            ResultTy::Bool,
        ),
        Shape::Param => Outcome::Err {
            code: "E0253",
            msg: Msg::NeedsBound {
                op,
                slot: Slot::Lhs,
            },
        },
        Shape::Poison => Outcome::Poison,
        _ => Outcome::Err {
            code: op.code(),
            msg: Msg::NoOperator {
                op,
                slot: Slot::Lhs,
            },
        },
    }
}

// -------------------------------------------------------------- shell

impl Checker<'_> {
    /// Describe `t` for the ladder, resolving the operator-specific facts
    /// (`proto`, `structural`) that the pure layer needs.
    pub(crate) fn operand(&mut self, op: Op, t: &Type) -> Operand {
        let t = self.resolve(t);
        let shape = match &t {
            Type::Int => Shape::Int,
            Type::Float => Shape::Float,
            Type::Str => Shape::Str,
            Type::Bool => Shape::Bool,
            Type::Char => Shape::Char,
            Type::Unit => Shape::Unit,
            Type::Var(_) => Shape::Var,
            Type::Error | Type::Never => Shape::Poison,
            Type::Named(id) if self.out.defs.is_quantity(*id) => Shape::Quantity {
                def: *id,
                float_base: self.base_of(*id) == Type::Float,
            },
            Type::Named(_) => Shape::Named,
            Type::Param(_) => Shape::Param,
            Type::Option(_) | Type::Result(..) | Type::List(_) | Type::Map(..) => Shape::Container,
            _ => Shape::Other,
        };

        let mut proto = None;
        let mut structural = false;
        match (&t, shape) {
            (Type::Named(def), Shape::Named) => match op {
                Op::Eq { .. } => {
                    proto = self.out.impl_maps.eq.get(&def.0).copied();
                    structural = self.named_has_eq(*def);
                }
                Op::Cmp(_) => {
                    proto = self.out.impl_maps.cmp.get(&def.0).copied();
                    structural = self.derives.get(def).is_some_and(|d| d.ord);
                }
                _ => {
                    if let Some(tr) = op.trait_id() {
                        proto = self.trait_impls.get(&(*def, tr)).map(|p| p[0]);
                    }
                }
            },
            (_, Shape::Container) => structural = op.is_relational() && self.eq_able(&t),
            (Type::Param(i), Shape::Param) => {
                structural = op.bound().is_some_and(|b| self.param_has_bound(*i, b));
            }
            _ => {}
        }
        Operand {
            shape,
            proto,
            structural,
        }
    }

    /// Apply an [`Outcome`]: record the lowering, or report the error.
    fn apply(
        &mut self,
        node: NodeId,
        span: Span,
        outcome: Outcome,
        lhs_ty: &Type,
        rhs_ty: Option<&Type>,
    ) -> Type {
        match outcome {
            Outcome::Lower(lowering, result) => {
                self.record(node, lowering);
                self.result_ty(result, lhs_ty, rhs_ty)
            }
            Outcome::DefaultInt(lowering, result) => {
                let t = self.resolve(lhs_ty);
                if self.infer.unify(&Type::Int, &t).is_ok() {
                    self.record(node, lowering);
                    match result {
                        ResultTy::Bool => Type::Bool,
                        _ => Type::Int,
                    }
                } else {
                    Type::Error
                }
            }
            Outcome::Err { code, msg } => {
                let names = Names {
                    lhs: self.ty_str(&self.resolve(lhs_ty)),
                    rhs: rhs_ty
                        .map(|t| self.ty_str(&self.resolve(t)))
                        .unwrap_or_default(),
                };
                let (message, help) = msg.render(&names);
                self.error_help(code, span, message, help);
                Type::Error
            }
            Outcome::Poison => Type::Error,
        }
    }

    fn record(&mut self, node: NodeId, lowering: Lowering) {
        self.out.set_lowering(node, lowering);
    }

    fn result_ty(&mut self, result: ResultTy, lhs_ty: &Type, rhs_ty: Option<&Type>) -> Type {
        match result {
            ResultTy::Bool => Type::Bool,
            ResultTy::Operand => self.resolve(lhs_ty),
            ResultTy::Rhs => match rhs_ty {
                Some(t) => self.resolve(t),
                None => self.resolve(lhs_ty),
            },
            ResultTy::Base => self.backing_type(lhs_ty),
        }
    }

    /// Check a binary operator application end to end.
    pub(crate) fn check_operator(&mut self, e: &Expr, op: Op, lhs: &Expr, rhs: &Expr) -> Type {
        let lt = self.check_expr(lhs, None);
        let why = if op.is_relational() {
            "both sides of a comparison must have the same type"
        } else {
            "arithmetic requires both operands to have the same type \
             (use `int(x)` / `float(x)` to convert)"
        };
        self.operator_over(e.id, e.span, op, lt, rhs, why)
    }

    /// Compound assignment (`place op= value`): the place's type is
    /// already known, and the operator runs between it and `value`. Unit
    /// places follow the same relaxed operand rules as the binary form, so
    /// `d *= 2` and `d += 500ms` both work.
    pub(crate) fn check_compound(
        &mut self,
        node: NodeId,
        span: Span,
        op: BinOp,
        place_ty: &Type,
        value: &Expr,
    ) -> Type {
        self.operator_over(
            node,
            span,
            Op::Arith(op),
            place_ty.clone(),
            value,
            "compound assignment requires the value to match the place's type \
             (use `int(x)` / `float(x)` to convert)",
        )
    }

    /// The shared body: `lt` is already checked, `rhs` is not.
    fn operator_over(
        &mut self,
        node: NodeId,
        span: Span,
        op: Op,
        lt: Type,
        rhs: &Expr,
        mismatch: &str,
    ) -> Type {
        let a = self.operand(op, &lt);

        // Both the expectation and the pairing rule come from the table —
        // they are operator knowledge, and deciding them here from a shape
        // test is what previously conflated `D * n` (scaling) with `D + D'`
        // (mixed families).
        let (expect, unify) = rhs_rule(op, &a);
        let rt = match expect {
            Expect::Free => self.check_expr(rhs, None),
            Expect::SameAsLhs => self.check_expr(rhs, Some(&lt)),
            Expect::Base => {
                let base = self.backing_type(&lt);
                self.check_expr(rhs, Some(&base))
            }
        };
        let b = self.operand(op, &rt);

        let unify = post_unify(op, &a, &b).unwrap_or(unify);
        match unify {
            Unify::SameType => {
                self.unify_or_err(&lt, &rt, rhs.span, mismatch);
            }
            Unify::SameTypeBecause(why) => {
                self.unify_or_err(&lt, &rt, rhs.span, why);
            }
            Unify::Backing { number, why } => match number {
                // `D * n` / `D / n`: the divisor or scale must be the
                // family's backing number.
                Slot::Rhs => {
                    let base = self.backing_type(&lt);
                    self.unify_or_err(&base, &rt, rhs.span, why);
                }
                // `n * D`: it is the *left* operand that must match the
                // right operand's family, and the whole expression is the
                // span that reads correctly.
                Slot::Lhs => {
                    let base = self.backing_type(&rt);
                    self.unify_or_err(&base, &lt, span, why);
                }
            },
            Unify::Nothing => {}
        }

        // Relational operators compare quantities as the number they are
        // stored in; the unify above has ruled out mixing families.
        let (a, ladder_ty) = if op.is_relational() {
            let backing = self.backing_type(&lt);
            (self.operand(op, &backing), backing)
        } else {
            (a, lt.clone())
        };

        let outcome = lower(op, &a, Some(&b));
        self.apply(node, span, outcome, &ladder_ty, Some(&rt))
    }

    /// Unary `-`. `!` is bool-only and stays in `check_expr`.
    pub(crate) fn check_neg(&mut self, e: &Expr, operand: &Expr) -> Type {
        let t = self.check_expr(operand, None);
        let a = self.operand(Op::Neg, &t);
        let outcome = lower(Op::Neg, &a, None);
        self.apply(e.id, e.span, outcome, &t, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(shape: Shape) -> Operand {
        Operand::of(shape)
    }

    fn with_proto(shape: Shape, proto: u32) -> Operand {
        Operand {
            shape,
            proto: Some(proto),
            structural: false,
        }
    }

    fn structural(shape: Shape) -> Operand {
        Operand {
            shape,
            proto: None,
            structural: true,
        }
    }

    const ADD: Op = Op::Arith(BinOp::Add);
    const MUL: Op = Op::Arith(BinOp::Mul);
    const DIV: Op = Op::Arith(BinOp::Div);
    const EQ: Op = Op::Eq { negate: false };
    const LT: Op = Op::Cmp(BinOp::Lt);

    // ------------------------------------------------------- arithmetic

    #[test]
    fn int_and_float_arithmetic_pick_their_primitive() {
        assert!(matches!(
            lower(ADD, &op(Shape::Int), Some(&op(Shape::Int))),
            Outcome::Lower(
                Lowering::BinOp(BinOpKind::IntArith(BinOp::Add)),
                ResultTy::Operand
            )
        ));
        assert!(matches!(
            lower(ADD, &op(Shape::Float), Some(&op(Shape::Float))),
            Outcome::Lower(Lowering::BinOp(BinOpKind::FloatArith(BinOp::Add)), _)
        ));
    }

    #[test]
    fn string_concatenates_only_with_plus() {
        assert!(matches!(
            lower(ADD, &op(Shape::Str), Some(&op(Shape::Str))),
            Outcome::Lower(Lowering::BinOp(BinOpKind::Concat), _)
        ));
        assert!(matches!(
            lower(
                Op::Arith(BinOp::Sub),
                &op(Shape::Str),
                Some(&op(Shape::Str))
            ),
            Outcome::Err { code: "E0234", .. }
        ));
    }

    /// Arithmetic defaults an unconstrained operand to int; `==` does not.
    /// The two ladders genuinely disagree here, and the table records it.
    #[test]
    fn var_defaults_to_int_for_arithmetic_but_not_for_equality() {
        assert!(matches!(
            lower(ADD, &op(Shape::Var), Some(&op(Shape::Var))),
            Outcome::DefaultInt(..)
        ));
        assert!(matches!(
            lower(EQ, &op(Shape::Var), Some(&op(Shape::Var))),
            Outcome::Lower(Lowering::BinOp(BinOpKind::EqValue { .. }), ResultTy::Bool)
        ));
    }

    #[test]
    fn a_named_type_needs_an_operator_impl() {
        assert!(matches!(
            lower(ADD, &with_proto(Shape::Named, 7), Some(&op(Shape::Named))),
            Outcome::Lower(Lowering::BinOp(BinOpKind::ArithCall { proto: 7 }), _)
        ));
        assert!(matches!(
            lower(ADD, &op(Shape::Named), Some(&op(Shape::Named))),
            Outcome::Err {
                code: "E0234",
                msg: Msg::NeedsTrait { .. }
            }
        ));
    }

    /// Arithmetic on a type parameter is unsupported in v1 regardless of
    /// bounds, where `==` and `<` succeed with one.
    #[test]
    fn type_parameters_never_do_arithmetic() {
        assert!(matches!(
            lower(ADD, &structural(Shape::Param), Some(&op(Shape::Param))),
            Outcome::Err {
                code: "E0253",
                msg: Msg::NoArithOnParam { .. }
            }
        ));
        assert!(matches!(
            lower(EQ, &structural(Shape::Param), Some(&op(Shape::Param))),
            Outcome::Lower(..)
        ));
        assert!(matches!(
            lower(EQ, &op(Shape::Param), Some(&op(Shape::Param))),
            Outcome::Err {
                code: "E0253",
                msg: Msg::NeedsBound { .. }
            }
        ));
    }

    // ---------------------------------------------------- unit families

    #[test]
    fn same_family_arithmetic_lowers_to_the_backing_number() {
        let d = op(Shape::Quantity {
            def: DefId(99),
            float_base: false,
        });
        assert!(matches!(
            lower(ADD, &d, Some(&d)),
            Outcome::Lower(
                Lowering::BinOp(BinOpKind::IntArith(BinOp::Add)),
                ResultTy::Operand
            )
        ));
    }

    #[test]
    fn a_quantity_divided_by_its_family_is_a_plain_number() {
        let d = op(Shape::Quantity {
            def: DefId(99),
            float_base: false,
        });
        assert!(matches!(
            lower(DIV, &d, Some(&d)),
            Outcome::Lower(_, ResultTy::Base)
        ));
    }

    #[test]
    fn a_number_may_scale_a_quantity_but_not_add_to_it() {
        let d = op(Shape::Quantity {
            def: DefId(99),
            float_base: false,
        });
        assert!(matches!(
            lower(MUL, &op(Shape::Int), Some(&d)),
            Outcome::Lower(_, ResultTy::Rhs)
        ));
        assert!(matches!(
            lower(ADD, &op(Shape::Int), Some(&d)),
            Outcome::Err {
                msg: Msg::ScaleOnly { .. },
                ..
            }
        ));
    }

    #[test]
    fn two_quantities_cannot_be_multiplied() {
        let d = op(Shape::Quantity {
            def: DefId(99),
            float_base: false,
        });
        assert!(matches!(
            lower(MUL, &d, Some(&d)),
            Outcome::Err {
                msg: Msg::NoDerivedDimension { .. },
                ..
            }
        ));
    }

    /// The pairing rule is operator knowledge, not the shell's. Deciding
    /// it from a shape test instead is what conflated `D * n` (scaling,
    /// which must not unify the two sides) with `D + D` (same family,
    /// which must).
    #[test]
    fn the_table_owns_how_operands_pair() {
        let d = op(Shape::Quantity {
            def: DefId(99),
            float_base: false,
        });
        assert_eq!(
            rhs_rule(ADD, &d),
            (Expect::SameAsLhs, Unify::SameTypeBecause(SAME_FAMILY))
        );
        assert_eq!(
            rhs_rule(MUL, &d),
            (
                Expect::Base,
                Unify::Backing {
                    number: Slot::Rhs,
                    why: SCALE_NEEDS_BACKING
                }
            )
        );
        assert_eq!(rhs_rule(DIV, &d), (Expect::Free, Unify::Nothing));
        assert_eq!(rhs_rule(EQ, &d), (Expect::SameAsLhs, Unify::SameType));
        // A plain number on the left must be checked freely so `n * D` is
        // discoverable at all; `post_unify` then restores same-type
        // arithmetic once the right side turns out not to be a quantity.
        assert_eq!(
            rhs_rule(MUL, &op(Shape::Int)),
            (Expect::Free, Unify::Nothing)
        );
        assert_eq!(
            post_unify(ADD, &op(Shape::Int), &op(Shape::Float)),
            Some(Unify::SameType)
        );
    }

    /// `D * D` is rejected by the ladder; unifying as well would stack a
    /// spurious E0220 behind the real diagnostic.
    #[test]
    fn a_ladder_level_pairing_error_suppresses_unification() {
        let d = op(Shape::Quantity {
            def: DefId(99),
            float_base: false,
        });
        assert_eq!(post_unify(MUL, &d, &d), Some(Unify::Nothing));
    }

    // -------------------------------------------------------- poisoning

    #[test]
    fn a_poisoned_left_operand_propagates_for_arithmetic() {
        assert!(matches!(
            lower(ADD, &op(Shape::Poison), Some(&op(Shape::Int))),
            Outcome::Poison
        ));
    }

    /// A poisoned *right* operand must not poison the result: the shell's
    /// unify already absorbed it, and turning `int + <error>` into `error`
    /// would suppress every check downstream of it.
    #[test]
    fn a_poisoned_right_operand_does_not_change_the_lowering() {
        assert!(matches!(
            lower(ADD, &op(Shape::Int), Some(&op(Shape::Poison))),
            Outcome::Lower(Lowering::BinOp(BinOpKind::IntArith(BinOp::Add)), _)
        ));
    }

    /// `==` accepts a poisoned operand rather than propagating, matching
    /// the pre-refactor behaviour where `Error | Never | Var` were one arm.
    #[test]
    fn equality_accepts_a_poisoned_operand() {
        assert!(matches!(
            lower(EQ, &op(Shape::Poison), None),
            Outcome::Lower(Lowering::BinOp(BinOpKind::EqValue { .. }), _)
        ));
    }

    // ------------------------------------------------------- relational

    #[test]
    fn unit_values_cannot_be_compared() {
        assert!(matches!(
            lower(EQ, &op(Shape::Unit), Some(&op(Shape::Unit))),
            Outcome::Err {
                msg: Msg::CannotCompareUnit,
                ..
            }
        ));
    }

    #[test]
    fn containers_compare_when_their_elements_do() {
        assert!(matches!(
            lower(EQ, &structural(Shape::Container), None),
            Outcome::Lower(..)
        ));
        assert!(matches!(
            lower(EQ, &op(Shape::Container), None),
            Outcome::Err {
                msg: Msg::ElementsUnsupported { .. },
                ..
            }
        ));
    }

    #[test]
    fn ordering_needs_ord_not_merely_eq() {
        assert!(matches!(
            lower(LT, &with_proto(Shape::Named, 3), None),
            Outcome::Lower(Lowering::BinOp(BinOpKind::CmpCall { proto: 3, .. }), _)
        ));
        assert!(matches!(
            lower(LT, &op(Shape::Named), None),
            Outcome::Err {
                msg: Msg::NeedsOrd { .. },
                ..
            }
        ));
    }

    #[test]
    fn functions_support_identity_only() {
        assert!(matches!(
            lower(EQ, &op(Shape::Other), None),
            Outcome::Err {
                msg: Msg::IdentityOnly { .. },
                ..
            }
        ));
    }

    // --------------------------------------------------------- messages

    #[test]
    fn messages_render_with_the_named_operand() {
        let names = Names {
            lhs: "Duration".to_string(),
            rhs: "Size".to_string(),
        };
        let (msg, help) = Msg::NeedsTrait {
            op: ADD,
            slot: Slot::Lhs,
        }
        .render(&names);
        assert_eq!(msg, "no `+` operator for `Duration`");
        assert!(help.contains("impl Add for Duration"), "{help}");

        let (msg, _) = Msg::CrossFamilyDivide {
            a: Slot::Lhs,
            b: Slot::Rhs,
        }
        .render(&names);
        assert_eq!(msg, "cannot divide `Duration` by `Size`");
    }
}
