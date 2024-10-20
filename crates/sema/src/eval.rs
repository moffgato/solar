use crate::{hir, ty::Gcx};
use alloy_primitives::U256;
use solar_ast::ast::LitKind;
use solar_interface::{diagnostics::ErrorGuaranteed, Span};
use std::fmt;

const RECURSION_LIMIT: usize = 64;

// TODO: `convertType` for truncating and extending correctly: https://github.com/ethereum/solidity/blob/de1a017ccb935d149ed6bcbdb730d89883f8ce02/libsolidity/analysis/ConstantEvaluator.cpp#L234

/// Evaluates simple constants.
pub struct ConstantEvaluator<'gcx> {
    pub gcx: Gcx<'gcx>,
    depth: usize,
}

type EvalResult<'gcx> = Result<IntScalar, EvalError>;

impl<'gcx> ConstantEvaluator<'gcx> {
    pub fn new(gcx: Gcx<'gcx>) -> Self {
        Self { gcx, depth: 0 }
    }

    pub fn eval(&mut self, expr: &hir::Expr<'_>) -> Result<IntScalar, ErrorGuaranteed> {
        self.eval_expr(expr).map_err(|err| match err.kind {
            EE::AlreadyEmitted(guar) => guar,
            _ => {
                let msg = "evaluation of constant value failed";
                self.gcx.dcx().err(msg).span(expr.span).span_note(err.span, err.kind.msg()).emit()
            }
        })
    }

    fn eval_expr(&mut self, expr: &hir::Expr<'_>) -> EvalResult<'gcx> {
        self.depth += 1;
        if self.depth > RECURSION_LIMIT {
            return Err(EE::RecursionLimitReached.spanned(expr.span));
        }
        let mut res = self.eval_expr_inner(expr);
        if let Err(e) = &mut res {
            if e.span.is_dummy() {
                e.span = expr.span;
            }
        }
        self.depth -= 1;
        res
    }

    fn eval_expr_inner(&mut self, expr: &hir::Expr<'_>) -> EvalResult<'gcx> {
        let expr = expr.peel_parens();
        match expr.kind {
            // hir::ExprKind::Array(_) => todo!(),
            // hir::ExprKind::Assign(_, _, _) => todo!(),
            hir::ExprKind::Binary(l, bin_op, r) => {
                let l = self.eval_expr(l)?;
                let r = self.eval_expr(r)?;
                l.binop(&r, bin_op.kind).map_err(Into::into)
            }
            // hir::ExprKind::Call(_, _) => todo!(),
            // hir::ExprKind::CallOptions(_, _) => todo!(),
            // hir::ExprKind::Delete(_) => todo!(),
            hir::ExprKind::Ident(&[hir::Res::Item(hir::ItemId::Variable(v))]) => {
                let v = self.gcx.hir.variable(v);
                if v.mutability != Some(hir::VarMut::Constant) {
                    return Err(EE::NonConstantVar.into());
                }
                self.eval_expr(v.initializer.expect("constant variable has no initializer"))
            }
            // hir::ExprKind::Index(_, _) => todo!(),
            // hir::ExprKind::Slice(_, _, _) => todo!(),
            hir::ExprKind::Lit(lit) => self.eval_lit(lit),
            // hir::ExprKind::Member(_, ident) => todo!(),
            // hir::ExprKind::New(_) => todo!(),
            // hir::ExprKind::Payable(_) => todo!(),
            hir::ExprKind::Ternary(cond, t, f) => {
                let cond = self.eval_expr(cond)?;
                Ok(if cond.to_bool() { self.eval_expr(t)? } else { self.eval_expr(f)? })
            }
            // hir::ExprKind::Tuple(_) => todo!(),
            // hir::ExprKind::TypeCall(_) => todo!(),
            // hir::ExprKind::Type(_) => todo!(),
            hir::ExprKind::Unary(un_op, v) => {
                let v = self.eval_expr(v)?;
                v.unop(un_op.kind).map_err(Into::into)
            }
            hir::ExprKind::Err(guar) => Err(EE::AlreadyEmitted(guar).into()),
            _ => Err(EE::UnsupportedExpr.into()),
        }
    }

    fn eval_lit(&mut self, lit: &hir::Lit) -> EvalResult<'gcx> {
        match lit.kind {
            // LitKind::Str(str_kind, arc) => todo!(),
            LitKind::Number(ref big_int) => {
                let (_, bytes) = big_int.to_bytes_be();
                if bytes.len() > 32 {
                    return Err(EE::IntTooBig.into());
                }
                Ok(IntScalar::from_be_bytes(&bytes))
            }
            // LitKind::Rational(ratio) => todo!(),
            LitKind::Address(address) => Ok(IntScalar::from_be_bytes(address.as_slice())),
            LitKind::Bool(bool) => Ok(IntScalar::from_be_bytes(&[bool as u8])),
            LitKind::Err(guar) => Err(EE::AlreadyEmitted(guar).into()),
            _ => Err(EE::UnsupportedLiteral.into()),
        }
    }
}

pub struct IntScalar {
    pub data: U256,
}

impl IntScalar {
    pub fn new(data: U256) -> Self {
        Self { data }
    }

    /// Creates a new integer value from a boolean.
    pub fn from_bool(value: bool) -> Self {
        Self { data: U256::from(value as u8) }
    }

    /// Creates a new integer value from big-endian bytes.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` is empty or has a length greater than 32.
    pub fn from_be_bytes(bytes: &[u8]) -> Self {
        Self { data: U256::from_be_slice(bytes) }
    }

    /// Converts the integer value to a boolean.
    pub fn to_bool(&self) -> bool {
        !self.data.is_zero()
    }

    /// Applies the given unary operation to this value.
    pub fn unop(&self, op: hir::UnOpKind) -> Result<Self, EE> {
        Ok(match op {
            hir::UnOpKind::PreInc
            | hir::UnOpKind::PreDec
            | hir::UnOpKind::PostInc
            | hir::UnOpKind::PostDec => return Err(EE::UnsupportedUnaryOp),
            hir::UnOpKind::Not | hir::UnOpKind::BitNot => Self::new(!self.data),
            hir::UnOpKind::Neg => Self::new(self.data.wrapping_neg()),
        })
    }

    /// Applies the given binary operation to this value.
    pub fn binop(&self, r: &Self, op: hir::BinOpKind) -> Result<Self, EE> {
        let l = self;
        Ok(match op {
            hir::BinOpKind::Lt => Self::from_bool(l.data < r.data),
            hir::BinOpKind::Le => Self::from_bool(l.data <= r.data),
            hir::BinOpKind::Gt => Self::from_bool(l.data > r.data),
            hir::BinOpKind::Ge => Self::from_bool(l.data >= r.data),
            hir::BinOpKind::Eq => Self::from_bool(l.data == r.data),
            hir::BinOpKind::Ne => Self::from_bool(l.data != r.data),
            hir::BinOpKind::Or | hir::BinOpKind::BitOr => Self::new(l.data | r.data),
            hir::BinOpKind::And | hir::BinOpKind::BitAnd => Self::new(l.data & r.data),
            hir::BinOpKind::BitXor => Self::new(l.data ^ r.data),
            hir::BinOpKind::Shr => {
                Self::new(l.data.wrapping_shr(r.data.try_into().unwrap_or(usize::MAX)))
            }
            hir::BinOpKind::Shl => {
                Self::new(l.data.wrapping_shl(r.data.try_into().unwrap_or(usize::MAX)))
            }
            hir::BinOpKind::Sar => {
                Self::new(l.data.arithmetic_shr(r.data.try_into().unwrap_or(usize::MAX)))
            }
            hir::BinOpKind::Add => {
                Self::new(l.data.checked_add(r.data).ok_or(EE::ArithmeticOverflow)?)
            }
            hir::BinOpKind::Sub => {
                Self::new(l.data.checked_sub(r.data).ok_or(EE::ArithmeticOverflow)?)
            }
            hir::BinOpKind::Pow => {
                Self::new(l.data.checked_pow(r.data).ok_or(EE::ArithmeticOverflow)?)
            }
            hir::BinOpKind::Mul => {
                Self::new(l.data.checked_mul(r.data).ok_or(EE::ArithmeticOverflow)?)
            }
            hir::BinOpKind::Div => Self::new(l.data.checked_div(r.data).ok_or(EE::DivisionByZero)?),
            hir::BinOpKind::Rem => Self::new(l.data.checked_rem(r.data).ok_or(EE::DivisionByZero)?),
        })
    }
}

#[derive(Debug)]
pub enum EvalErrorKind {
    RecursionLimitReached,
    ArithmeticOverflow,
    IntTooBig,
    DivisionByZero,
    UnsupportedLiteral,
    UnsupportedUnaryOp,
    UnsupportedExpr,
    NonConstantVar,
    AlreadyEmitted(ErrorGuaranteed),
}
use EvalErrorKind as EE;

impl EvalErrorKind {
    pub fn spanned(self, span: Span) -> EvalError {
        EvalError { kind: self, span }
    }

    fn msg(&self) -> &'static str {
        match self {
            Self::RecursionLimitReached => "recursion limit reached",
            Self::ArithmeticOverflow => "arithmetic overflow",
            Self::IntTooBig => "integer value is too big",
            Self::DivisionByZero => "division by zero",
            Self::UnsupportedLiteral => "unsupported literal",
            Self::UnsupportedUnaryOp => "unsupported unary operation",
            Self::UnsupportedExpr => "unsupported expression",
            Self::NonConstantVar => "only constant variables are allowed",
            Self::AlreadyEmitted(_) => "error already emitted",
        }
    }
}

#[derive(Debug)]
pub struct EvalError {
    pub span: Span,
    pub kind: EvalErrorKind,
}

impl From<EE> for EvalError {
    fn from(value: EE) -> Self {
        Self { kind: value, span: Span::DUMMY }
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.msg().fmt(f)
    }
}

impl std::error::Error for EvalError {}