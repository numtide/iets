//! POSIX extended regular expressions for `builtins.match` and `builtins.split`.
//!
//! Written from the observed behaviour of Nix's `builtins.match` and
//! `builtins.split` (probed as a black box with `nix-instantiate`) and from
//! the POSIX ERE specification; it is not derived from libstdc++. Where Nix
//! departs from POSIX leftmost-longest, the observed rules are:
//!
//! - A search takes the leftmost start. From there every path is tried in
//!   priority order and the longest end wins; ties go to the first path.
//! - A greedy quantifier does not try fewer repetitions once the path with
//!   more repetitions reached any match, even a shorter one.
//! - A loop iteration that consumes nothing ends the loop, so `(a*)*` on
//!   "aa" captures "". Captures are not reset between iterations.
//! - `builtins.match` takes the first path in priority order that ends at
//!   the end of the input.
//! - Ranges compare bytes as signed chars; classes are the C locale's.
//! - The pattern size limit counts states as `Parser::piece` does.

use std::{array, iter, mem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    TooBig,
}

type Res<T> = Result<T, Error>;
type ByteSet = [bool; 256];

const STATE_LIMIT: u64 = 99_995;
const NONE: usize = usize::MAX;

/// Portable character set names in code order; letters name themselves.
const NAMES: &str = "NUL SOH STX ETX EOT ENQ ACK alert backspace tab newline vertical-tab form-feed \
    carriage-return SO SI DLE DC1 DC2 DC3 DC4 NAK SYN ETB CAN EM SUB ESC IS4 IS3 IS2 IS1 \
    space exclamation-mark quotation-mark number-sign dollar-sign percent-sign ampersand \
    apostrophe left-parenthesis right-parenthesis asterisk plus-sign comma hyphen period \
    slash zero one two three four five six seven eight nine colon semicolon less-than-sign \
    equals-sign greater-than-sign question-mark commercial-at A B C D E F G H I J K L M N \
    O P Q R S T U V W X Y Z left-square-bracket backslash right-square-bracket circumflex \
    underscore grave-accent a b c d e f g h i j k l m n o p q r s t u v w x y z \
    left-curly-bracket vertical-line right-curly-bracket tilde \
    DEL";

enum Quant {
    Star,
    Plus,
    Opt,
    Range(u32, Option<u32>),
}

enum Node {
    Byte(u8),
    Set(Box<ByteSet>),
    Any,
    Bol,
    Eol,
    Group(u32, Box<Node>),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Rep(Box<Node>, Quant),
}

struct Parser<'a> {
    re: &'a [u8],
    i: usize,
    groups: u32,
    depth: u32,
    states: u64,
    /// `std::regex::icase`: a letter matches in either case (the C locale's).
    icase: bool,
}

/// Both cases of every ASCII letter the set holds. A bracket folds before it
/// is negated, so `[^a]` matches neither case.
fn fold_case(set: &mut ByteSet) {
    for c in b'A'..=b'Z' {
        let (u, l) = (c as usize, c.to_ascii_lowercase() as usize);
        let both = set[u] | set[l];
        (set[u], set[l]) = (both, both);
    }
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.re.get(self.i).copied()
    }

    fn next(&mut self) -> Res<u8> {
        let c = self.peek().ok_or(Error::Invalid)?;
        self.i += 1;
        Ok(c)
    }

    fn alt(&mut self) -> Res<Node> {
        let mut branches = vec![self.branch()?];
        while self.peek() == Some(b'|') {
            self.i += 1;
            self.states += 3;
            branches.push(self.branch()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alt(branches)
        })
    }

    fn branch(&mut self) -> Res<Node> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'|') => break,
                Some(b')') if self.depth > 0 => break,
                _ => items.push(self.piece()?),
            }
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Node::Cat(items)
        })
    }

    /// An atom and its quantifiers. With `s` the states of the quantified
    /// piece so far: `*` and `+` add 1, `?` adds 2, `{n}` adds s*n+1,
    /// `{n,}` adds s*(n+1)+1 and `{m,n}` adds s*n+(n-m)+1.
    fn piece(&mut self) -> Res<Node> {
        let before = self.states;
        let (mut node, quantifiable) = self.atom()?;
        while let Some(q) = self.quant()? {
            if !quantifiable {
                return Err(Error::Invalid);
            }
            let s = self.states - before;
            self.states = self.states.saturating_add(match q {
                Quant::Star | Quant::Plus => 1,
                Quant::Opt => 2,
                Quant::Range(m, None) => s.saturating_mul(m as u64 + 1).saturating_add(1),
                Quant::Range(m, Some(n)) => s
                    .saturating_mul(n as u64)
                    .saturating_add((n - m) as u64 + 1),
            });
            if self.states > STATE_LIMIT {
                if self.peek() == Some(b'\\') && self.escape().is_err() {
                    return Err(Error::Invalid);
                }
                return Err(Error::TooBig);
            }
            node = Node::Rep(Box::new(node), q);
        }
        Ok(node)
    }

    fn atom(&mut self) -> Res<(Node, bool)> {
        let c = self.next()?;
        self.states += 1;
        let node = match c {
            b'(' => {
                let g = self.groups;
                self.groups += 1;
                self.depth += 1;
                self.states += 2;
                let inner = self.alt()?;
                if self.next()? != b')' {
                    return Err(Error::Invalid);
                }
                self.depth -= 1;
                Node::Group(g, Box::new(inner))
            }
            b')' | b'*' | b'+' | b'?' | b'{' => return Err(Error::Invalid),
            b'^' => return Ok((Node::Bol, false)),
            b'$' => return Ok((Node::Eol, false)),
            b'.' => Node::Any,
            b'[' => Node::Set(self.bracket()?),
            b'\\' => {
                self.i -= 1;
                let c = self.escape()?;
                self.literal(c)
            }
            c => self.literal(c),
        };
        Ok((node, true))
    }

    /// One literal byte, as a set when it must match either case.
    fn literal(&self, c: u8) -> Node {
        if !self.icase || !c.is_ascii_alphabetic() {
            return Node::Byte(c);
        }
        let mut set = Box::new([false; 256]);
        set[c as usize] = true;
        fold_case(&mut set);
        Node::Set(set)
    }

    fn escape(&mut self) -> Res<u8> {
        self.i += 1;
        let c = self.next()?;
        if b"$()*+.?[\\^{|".contains(&c) {
            Ok(c)
        } else {
            Err(Error::Invalid)
        }
    }

    fn quant(&mut self) -> Res<Option<Quant>> {
        let q = match self.peek() {
            Some(b'*') => Quant::Star,
            Some(b'+') => Quant::Plus,
            Some(b'?') => Quant::Opt,
            Some(b'{') => {
                self.i += 1;
                let m = self.number()?.ok_or(Error::Invalid)?;
                let n = if self.peek() == Some(b',') {
                    self.i += 1;
                    self.number()?
                } else {
                    Some(m)
                };
                if self.peek() != Some(b'}') || n.is_some_and(|n| n < m) {
                    return Err(Error::Invalid);
                }
                Quant::Range(m, n)
            }
            _ => return Ok(None),
        };
        self.i += 1;
        Ok(Some(q))
    }

    fn number(&mut self) -> Res<Option<u32>> {
        let start = self.i;
        let mut v: u64 = 0;
        while let Some(d @ b'0'..=b'9') = self.peek() {
            v = v * 10 + (d - b'0') as u64;
            if v > i32::MAX as u64 {
                return Err(Error::Invalid);
            }
            self.i += 1;
        }
        Ok((self.i > start).then_some(v as u32))
    }

    fn bracket(&mut self) -> Res<Box<ByteSet>> {
        let mut set = Box::new([false; 256]);
        let negate = self.peek() == Some(b'^');
        if negate {
            self.i += 1;
        }
        let mut first = true;
        let mut prev: Option<u8> = None;
        loop {
            let c = self.next()?;
            match c {
                b']' if !first => break,
                b'[' if matches!(self.peek(), Some(b':' | b'=' | b'.')) => {
                    let kind = self.next()?;
                    let name = self.bracket_name(kind)?;
                    prev = None;
                    match kind {
                        b':' => add_class(&mut set, name)?,
                        b'=' => {
                            let b = named_char(name)?;
                            set[b.to_ascii_lowercase() as usize] = true;
                            set[b.to_ascii_uppercase() as usize] = true;
                        }
                        _ => {
                            let b = named_char(name)?;
                            set[b as usize] = true;
                            prev = Some(b);
                        }
                    }
                }
                b'-' if !first && self.peek() != Some(b']') => {
                    let lo = prev.take().ok_or(Error::Invalid)?;
                    let hi = self.next()?;
                    if hi == b'[' && matches!(self.peek(), Some(b':' | b'=' | b'.')) {
                        return Err(Error::Invalid);
                    }
                    if (lo as i8) > (hi as i8) {
                        return Err(Error::Invalid);
                    }
                    for b in 0..=255u8 {
                        if (lo as i8..=hi as i8).contains(&(b as i8)) {
                            set[b as usize] = true;
                        }
                    }
                }
                c => {
                    set[c as usize] = true;
                    prev = Some(c);
                }
            }
            first = false;
        }
        if self.icase {
            fold_case(&mut set);
        }
        if negate {
            for b in set.iter_mut() {
                *b = !*b;
            }
        }
        Ok(set)
    }

    fn bracket_name(&mut self, kind: u8) -> Res<&[u8]> {
        let start = self.i;
        let rest = &self.re[start..];
        let len = rest
            .windows(2)
            .position(|w| w == [kind, b']'])
            .ok_or(Error::Invalid)?;
        self.i = start + len + 2;
        Ok(&self.re[start..start + len])
    }
}

fn named_char(name: &[u8]) -> Res<u8> {
    NAMES
        .split(' ')
        .position(|n| n.as_bytes() == name)
        .map(|i| i as u8)
        .ok_or(Error::Invalid)
}

fn add_class(set: &mut ByteSet, name: &[u8]) -> Res<()> {
    let f: fn(&u8) -> bool = match name.to_ascii_lowercase().as_slice() {
        b"alnum" => u8::is_ascii_alphanumeric,
        b"alpha" => u8::is_ascii_alphabetic,
        b"blank" => |c| *c == b' ' || *c == b'\t',
        b"cntrl" => u8::is_ascii_control,
        b"digit" | b"d" => u8::is_ascii_digit,
        b"graph" => u8::is_ascii_graphic,
        b"lower" => u8::is_ascii_lowercase,
        b"print" => |c| c.is_ascii_graphic() || *c == b' ',
        b"punct" => u8::is_ascii_punctuation,
        b"space" | b"s" => |c| matches!(c, b' ' | b'\t'..=b'\r'),
        b"upper" => u8::is_ascii_uppercase,
        b"xdigit" => u8::is_ascii_hexdigit,
        b"w" => |c| c.is_ascii_alphanumeric() || *c == b'_',
        _ => return Err(Error::Invalid),
    };
    for b in 0..=255u8 {
        if f(&b) {
            set[b as usize] = true;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Inst {
    Byte(u8),
    Set(u32),
    Any,
    Bol,
    Eol,
    /// Write the position to a register (capture bound or loop mark).
    Save(u32),
    Clear(u32),
    /// Run the next instruction, then on failure the target.
    Fork(u32),
    Jmp(u32),
    /// Greedy: run the next instruction; the target runs only if that
    /// found no match at all.
    Opt(u32),
    /// Loop back to `body` unless the iteration that started at the mark
    /// consumed nothing; the exit is the next instruction, as for `Opt`.
    Loop {
        mark: u32,
        body: u32,
    },
    /// Greedy repetition of a one-byte set, `min1` for `+`.
    Star {
        set: u32,
        min1: bool,
    },
    Match,
}

pub struct Prog {
    insts: Vec<Inst>,
    sets: Vec<ByteSet>,
    ncaps: usize,
    nregs: usize,
    /// Bytes a non-empty match can start with; `None` if it can be empty.
    first: Option<Box<ByteSet>>,
    anchored: bool,
}

struct Compiler {
    insts: Vec<Inst>,
    sets: Vec<ByteSet>,
    nregs: u32,
}

impl Compiler {
    fn push(&mut self, i: Inst) -> usize {
        self.insts.push(i);
        self.insts.len() - 1
    }

    fn here(&self) -> u32 {
        self.insts.len() as u32
    }

    fn patch(&mut self, at: usize) {
        let to = self.here();
        match &mut self.insts[at] {
            Inst::Fork(t) | Inst::Jmp(t) | Inst::Opt(t) => *t = to,
            _ => unreachable!(),
        }
    }

    fn set(&mut self, s: ByteSet) -> u32 {
        self.sets.push(s);
        (self.sets.len() - 1) as u32
    }

    fn emit(&mut self, n: &Node) {
        match n {
            Node::Byte(b) => _ = self.push(Inst::Byte(*b)),
            Node::Set(s) => {
                let s = self.set(**s);
                self.push(Inst::Set(s));
            }
            Node::Any => _ = self.push(Inst::Any),
            Node::Bol => _ = self.push(Inst::Bol),
            Node::Eol => _ = self.push(Inst::Eol),
            Node::Group(g, inner) => {
                self.push(Inst::Save(2 * g));
                self.emit(inner);
                self.push(Inst::Save(2 * g + 1));
            }
            Node::Cat(items) => items.iter().for_each(|i| self.emit(i)),
            Node::Alt(branches) => {
                let mut ends = Vec::new();
                for (k, b) in branches.iter().enumerate() {
                    if k + 1 == branches.len() {
                        self.emit(b);
                    } else {
                        let f = self.push(Inst::Fork(0));
                        self.emit(b);
                        ends.push(self.push(Inst::Jmp(0)));
                        self.patch(f);
                    }
                }
                ends.into_iter().for_each(|e| self.patch(e));
            }
            Node::Rep(inner, q) => match *q {
                Quant::Star => self.star(inner, false),
                Quant::Plus => self.star(inner, true),
                Quant::Opt => {
                    let o = self.push(Inst::Opt(0));
                    self.emit(inner);
                    self.patch(o);
                }
                Quant::Range(m, max) => {
                    (0..m).for_each(|_| self.emit(inner));
                    match max {
                        None => self.star(inner, false),
                        Some(n) => {
                            let opts: Vec<usize> = (m..n)
                                .map(|_| {
                                    let o = self.push(Inst::Opt(0));
                                    self.emit(inner);
                                    o
                                })
                                .collect();
                            opts.into_iter().for_each(|o| self.patch(o));
                        }
                    }
                }
            },
        }
    }

    fn star(&mut self, inner: &Node, min1: bool) {
        let one = match inner {
            Node::Byte(b) => Some(array::from_fn(|c| c == *b as usize)),
            Node::Set(s) => Some(**s),
            Node::Any => Some(array::from_fn(|c| c != 0)),
            _ => None,
        };
        if let Some(s) = one {
            let set = self.set(s);
            self.push(Inst::Star { set, min1 });
            return;
        }
        let mark = self.nregs;
        self.nregs += 1;
        let mut skip = None;
        if !min1 {
            self.push(Inst::Clear(mark));
            skip = Some(self.push(Inst::Jmp(0)));
        }
        let body = self.here();
        self.push(Inst::Save(mark));
        self.emit(inner);
        if let Some(j) = skip {
            self.patch(j);
        }
        self.push(Inst::Loop { mark, body });
    }
}

/// Bytes that can start a match, or `None` if a match can be
/// empty. With `stop_at_bol`, paths through `^` are dropped.
fn first_bytes(insts: &[Inst], sets: &[ByteSet], stop_at_bol: bool) -> Option<ByteSet> {
    let mut out = [false; 256];
    let mut seen = vec![false; insts.len()];
    let mut work = vec![0usize];
    while let Some(pc) = work.pop() {
        if mem::replace(&mut seen[pc], true) {
            continue;
        }
        match insts[pc] {
            Inst::Byte(b) => out[b as usize] = true,
            Inst::Set(s) => (0..256).for_each(|c| out[c] |= sets[s as usize][c]),
            Inst::Any => (1..256).for_each(|c| out[c] = true),
            Inst::Star { set, min1 } => {
                (0..256).for_each(|c| out[c] |= sets[set as usize][c]);
                if !min1 {
                    work.push(pc + 1);
                }
            }
            Inst::Bol if stop_at_bol => {}
            Inst::Bol | Inst::Save(_) | Inst::Clear(_) => work.push(pc + 1),
            Inst::Eol | Inst::Match => return None,
            Inst::Fork(t) | Inst::Opt(t) => work.extend([pc + 1, t as usize]),
            Inst::Jmp(t) => work.push(t as usize),
            Inst::Loop { body, .. } => work.extend([pc + 1, body as usize]),
        }
    }
    Some(out)
}

pub fn compile(re: &[u8]) -> Result<Prog, Error> {
    compile_opt(re, false)
}

/// `compile`, with `std::regex::icase` when `icase` (`nix search`).
pub fn compile_opt(re: &[u8], icase: bool) -> Result<Prog, Error> {
    let mut p = Parser {
        re,
        i: 0,
        groups: 0,
        depth: 0,
        states: 0,
        icase,
    };
    let node = p.alt()?;
    if p.i < re.len() {
        return Err(Error::Invalid);
    }
    let ncaps = 2 * p.groups as usize;
    let mut c = Compiler {
        insts: Vec::new(),
        sets: Vec::new(),
        nregs: ncaps as u32,
    };
    c.emit(&node);
    c.push(Inst::Match);
    let first = first_bytes(&c.insts, &c.sets, false).map(Box::new);
    let anchored = first_bytes(&c.insts, &c.sets, true).is_some_and(|s| !s.contains(&true));
    Ok(Prog {
        ncaps,
        nregs: c.nregs as usize,
        insts: c.insts,
        sets: c.sets,
        first,
        anchored,
    })
}

enum Frame {
    Fork(u32, usize),
    /// Exit to `pc` at `pos` unless a match was seen since the push.
    Opt(u32, usize, u64),
    /// As `Opt` for each exit position `lo..=hi`, highest first.
    Star(u32, usize, usize, u64),
    Reg(u32, usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Full,
    Search { not_null: bool },
}

struct Exec<'a> {
    prog: &'a Prog,
    hay: &'a [u8],
    regs: Vec<usize>,
    stack: Vec<Frame>,
    /// Matches reached so far; see `Frame::Opt`.
    seen: u64,
}

fn eat(ok: bool, pos: &mut usize) -> bool {
    *pos += ok as usize;
    ok
}

impl<'a> Exec<'a> {
    fn new(prog: &'a Prog, hay: &'a [u8]) -> Self {
        Exec {
            prog,
            hay,
            regs: vec![NONE; prog.nregs],
            stack: Vec::new(),
            seen: 0,
        }
    }

    fn set_reg(&mut self, reg: u32, v: usize) -> bool {
        let old = mem::replace(&mut self.regs[reg as usize], v);
        self.stack.push(Frame::Reg(reg, old));
        true
    }

    fn push(&mut self, f: Frame) -> bool {
        self.stack.push(f);
        true
    }

    /// The end and capture registers of the match starting at `start`.
    fn run(&mut self, start: usize, mode: Mode) -> Option<(usize, Vec<usize>)> {
        let (insts, sets, hay) = (&self.prog.insts, &self.prog.sets, self.hay);
        let len = hay.len();
        self.regs.fill(NONE);
        self.stack.clear();
        let mut best: Option<(usize, Vec<usize>)> = None;
        let (mut pc, mut pos) = (0usize, start);
        'run: loop {
            let ok = match insts[pc] {
                Inst::Byte(b) => eat(hay.get(pos) == Some(&b), &mut pos),
                Inst::Set(s) => eat(
                    hay.get(pos).is_some_and(|&c| sets[s as usize][c as usize]),
                    &mut pos,
                ),
                Inst::Any => eat(hay.get(pos).is_some_and(|&c| c != 0), &mut pos),
                Inst::Bol => pos == 0,
                Inst::Eol => pos == len,
                Inst::Save(r) => self.set_reg(r, pos),
                Inst::Clear(r) => self.set_reg(r, NONE),
                Inst::Fork(t) => self.push(Frame::Fork(t, pos)),
                Inst::Opt(t) => self.push(Frame::Opt(t, pos, self.seen)),
                Inst::Jmp(t) => {
                    pc = t as usize;
                    continue;
                }
                Inst::Loop { mark, body } => {
                    if self.regs[mark as usize] != pos {
                        self.stack.push(Frame::Opt(pc as u32 + 1, pos, self.seen));
                        pc = body as usize;
                        continue;
                    }
                    true
                }
                Inst::Star { set, min1 } => {
                    let s = &sets[set as usize];
                    let lo = pos + min1 as usize;
                    let end = pos + hay[pos..].iter().take_while(|&&b| s[b as usize]).count();
                    if end > lo {
                        self.stack
                            .push(Frame::Star(pc as u32 + 1, lo, end - 1, self.seen));
                    }
                    pos = end;
                    end >= lo
                }
                Inst::Match => match mode {
                    Mode::Full if pos == len => return Some((pos, self.regs.clone())),
                    Mode::Full => false,
                    Mode::Search { not_null } if not_null && pos == start => false,
                    Mode::Search { .. } => {
                        self.seen += 1;
                        if best.as_ref().is_none_or(|b| pos > b.0) {
                            best = Some((pos, self.regs.clone()));
                        }
                        if pos == len {
                            break 'run;
                        }
                        false
                    }
                },
            };
            if ok {
                pc += 1;
                continue;
            }
            loop {
                match self.stack.pop() {
                    None => break 'run,
                    Some(Frame::Reg(r, old)) => {
                        self.regs[r as usize] = old;
                        continue;
                    }
                    Some(Frame::Fork(t, p)) => (pc, pos) = (t as usize, p),
                    Some(Frame::Opt(t, p, seen)) if seen == self.seen => {
                        (pc, pos) = (t as usize, p)
                    }
                    Some(Frame::Star(t, lo, hi, seen)) if seen == self.seen => {
                        if hi > lo {
                            self.stack.push(Frame::Star(t, lo, hi - 1, seen));
                        }
                        (pc, pos) = (t as usize, hi);
                    }
                    Some(_) => continue,
                }
                break;
            }
        }
        best
    }

    fn search(&mut self, from: usize, continuous: bool, not_null: bool) -> Option<Match> {
        let len = self.hay.len();
        for start in from..=len {
            if self.prog.anchored && start > 0 {
                return None;
            }
            let viable = match &self.prog.first {
                Some(f) => start < len && f[self.hay[start] as usize],
                None => true,
            };
            if viable && let Some((end, regs)) = self.run(start, Mode::Search { not_null }) {
                return Some(Match {
                    start,
                    end,
                    groups: groups(self.prog, &regs),
                });
            }
            if continuous {
                return None;
            }
        }
        None
    }
}

fn groups(prog: &Prog, regs: &[usize]) -> Vec<Option<(usize, usize)>> {
    regs[..prog.ncaps]
        .chunks(2)
        .map(|g| (g[0] != NONE && g[1] != NONE && g[0] <= g[1]).then(|| (g[0], g[1])))
        .collect()
}

pub struct Match {
    pub start: usize,
    pub end: usize,
    pub groups: Vec<Option<(usize, usize)>>,
}

/// The capture groups when the whole of `hay` matches.
pub fn match_exact(prog: &Prog, hay: &[u8]) -> Option<Vec<Option<(usize, usize)>>> {
    if let Some(f) = &prog.first
        && hay.first().is_none_or(|&b| !f[b as usize])
    {
        return None;
    }
    let (_, regs) = Exec::new(prog, hay).run(0, Mode::Full)?;
    Some(groups(prog, &regs))
}

/// Successive matches in `std::regex_iterator` order: after an empty match
/// a non-empty one is tried at the same position before moving one byte on.
pub fn matches<'a>(prog: &'a Prog, hay: &'a [u8]) -> impl Iterator<Item = Match> + 'a {
    let mut ex = Exec::new(prog, hay);
    let mut next = ex.search(0, false, false);
    iter::from_fn(move || {
        let m = next.take()?;
        next = if m.start < m.end {
            ex.search(m.end, false, false)
        } else if m.end < hay.len() {
            ex.search(m.end, true, true)
                .or_else(|| ex.search(m.end + 1, false, false))
        } else {
            None
        };
        Some(m)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;
    use std::{fs, hint};

    fn caps(re: &str, s: &str) -> Option<Vec<Option<String>>> {
        let p = compile(re.as_bytes()).unwrap();
        match_exact(&p, s.as_bytes()).map(|g| {
            g.into_iter()
                .map(|g| g.map(|(a, b)| s[a..b].to_string()))
                .collect()
        })
    }

    #[test]
    fn long_inputs_do_not_recurse() {
        let s = "ab".repeat(100_000);
        assert_eq!(caps("(a|b)*", &s), Some(vec![Some("b".into())]));
        assert_eq!(caps("((a)(b))*x", &s), None);
        let p = compile(b"(a|b)").unwrap();
        assert_eq!(matches(&p, s.as_bytes()).count(), 200_000);
    }

    #[test]
    fn names() {
        assert_eq!(NAMES.split(' ').count(), 128);
        assert_eq!(named_char(b"hyphen"), Ok(b'-'));
        assert_eq!(named_char(b"tilde"), Ok(b'~'));
        assert_eq!(named_char(b"DEL"), Ok(127));
    }

    #[test]
    fn size_limit() {
        assert!(compile(b"a{99993}").is_ok());
        assert_eq!(compile(b"a{99994}").err(), Some(Error::TooBig));
        assert_eq!(compile(b"(a){24998}").err(), Some(Error::TooBig));
        assert_eq!(compile(b"a{99999}\\").err(), Some(Error::Invalid));
        assert_eq!(compile(b"a{2147483648}").err(), Some(Error::Invalid));
    }

    /// `REGEX_BENCH=1 cargo test --release regex_bench -- --ignored --nocapture`
    /// times the patterns nixos-desktop runs most, on the inputs it gives them.
    #[test]
    #[ignore]
    fn regex_bench() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/regex-bench.json");
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        for e in v.as_array().unwrap() {
            let (kind, re) = (e["kind"].as_str().unwrap(), e["re"].as_str().unwrap());
            let inputs: Vec<&[u8]> = e["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().as_bytes())
                .collect();
            let prog = compile(re.as_bytes()).unwrap();
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                for _ in 0..200 {
                    for s in &inputs {
                        if kind == "m" {
                            hint::black_box(match_exact(&prog, s));
                        } else {
                            hint::black_box(matches(&prog, s).count());
                        }
                    }
                }
                best = best.min(t.elapsed().as_nanos() as f64 / (200 * inputs.len()) as f64);
            }
            println!("{best:>9.1} ns  {kind}  {re:?}");
        }
    }
}
