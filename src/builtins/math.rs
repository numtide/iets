//! Arithmetic and comparison primops.

use super::Registry;
use crate::eval::{ArithOp, EvalResult, Evaluator, Pos};
use crate::runtime::Word;

pub fn register(r: &mut Registry) {
    r.op("__add", 2, add);
    r.op("__sub", 2, sub);
    r.op("__mul", 2, mul);
    r.op("__div", 2, div);
    r.op("__bitAnd", 2, bit_and);
    r.op("__bitOr", 2, bit_or);
    r.op("__bitXor", 2, bit_xor);
    r.op("__lessThan", 2, less_than);
}

fn add(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    ev.arith(args[0], args[1], ArithOp::Add, pos)
}

fn sub(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    ev.arith(args[0], args[1], ArithOp::Sub, pos)
}

fn mul(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    ev.arith(args[0], args[1], ArithOp::Mul, pos)
}

fn div(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    ev.arith(args[0], args[1], ArithOp::Div, pos)
}

fn bit_and(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_int(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.bitAnd",
    )?;
    let b = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.bitAnd",
    )?;
    Ok(ev.mk_int(a & b))
}

fn bit_or(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_int(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.bitOr",
    )?;
    let b = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.bitOr",
    )?;
    Ok(ev.mk_int(a | b))
}

fn bit_xor(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_int(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.bitXor",
    )?;
    let b = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.bitXor",
    )?;
    Ok(ev.mk_int(a ^ b))
}

fn less_than(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    Ok(Word::from_bool(ev.less_than(args[0], args[1], pos)?))
}
