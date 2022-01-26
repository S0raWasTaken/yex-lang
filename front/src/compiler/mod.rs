use crate::{
    error::ParseError,
    lexer::Lexer,
    tokens::{Token, TokenType as Tkt},
};
use std::{collections::HashMap, iter::Peekable, mem::take};
use vm::{
    gc::GcRef, stackvec, Bytecode, Constant, Either, Fun, List, OpCode, OpCodeMetadata, Symbol,
    Table,
};

type ParseResult = Result<(), ParseError>;

fn patch_bytecode(len: usize, bt_len: usize, bytecode: &[OpCodeMetadata]) -> Bytecode {
    bytecode
        .iter()
        .copied()
        .map(|mut it| {
            it.opcode = match it.opcode {
                OpCode::Push(idx) => OpCode::Push(idx + len),
                OpCode::Jmp(offset) => OpCode::Jmp(offset + bt_len),
                OpCode::Jmf(offset) => OpCode::Jmf(offset + bt_len),
                other => other,
            };
            it
        })
        .collect()
}

#[derive(Default)]
struct Proxy {
    vars: HashMap<Symbol, usize>,
    opcodes: Vec<OpCodeMetadata>,
}

pub struct Compiler {
    lexer: Peekable<Lexer>,
    constants: Vec<Constant>,
    current: Token,
    proxies: Vec<Proxy>,
}

impl Compiler {
    pub fn compile(lexer: Lexer) -> Result<(Bytecode, Vec<Constant>), ParseError> {
        let mut this = Self {
            lexer: lexer.peekable(),
            constants: vec![],
            current: Token {
                line: 0,
                column: 0,
                token: Tkt::Eof,
            },
            proxies: vec![Proxy::default()],
        };
        this.next()?;

        loop {
            match this.current.token {
                Tkt::Open => this.open(),
                _ => this.def(),
            }?;
            if this.current.token == Tkt::Eof {
                break;
            }
        }

        Ok((this.proxies.pop().unwrap().opcodes, this.constants))
    }

    pub fn compile_expr(lexer: Lexer) -> Result<(Bytecode, Vec<Constant>), ParseError> {
        let mut this = Self {
            lexer: lexer.peekable(),
            constants: vec![],
            current: Token {
                line: 0,
                column: 0,
                token: Tkt::Eof,
            },
            proxies: vec![Proxy::default()],
        };
        this.next()?;

        this.expression()?;
        if this.current.token != Tkt::Eof {
            this.throw(format!("Expected <eof>, got `{}`", this.current.token))?
        }
        Ok((this.proxies.pop().unwrap().opcodes, this.constants))
    }

    fn def(&mut self) -> ParseResult {
        if self.current.token != Tkt::Def {
            self.throw(format!("Expected `def`, found `{}`", self.current.token))?
        }
        self.next()?; // skips the let token

        let name = match take(&mut self.current.token) {
            Tkt::Name(v) => Symbol::new(v),
            o => return self.throw(format!("Expected variable name after `def`, found `{}`", o)),
        };
        self.next()?;

        if matches!(self.current.token, Tkt::Name(_)) {
            self.function()?;
        } else {
            self.consume(
                &[Tkt::Assign],
                format!("Expected `=` after name, found `{}`", self.current.token),
            )?;
            self.expression()?;
        }

        self.emit(OpCode::Savg(name));

        Ok(())
    }

    fn open(&mut self) -> ParseResult {
        assert_eq!(self.current.token, Tkt::Open);
        self.next()?;
        let file_name = match &self.current.token {
            Tkt::Str(ref s) => s,
            other => {
                return self.throw(format!("Expected file name after `open`, found {}", other))
            }
        };
        let file = match std::fs::read_to_string(&file_name) {
            Ok(f) => f,
            Err(_) => return self.throw(format!("File `{}` not found", file_name)),
        };
        self.next()?;

        match crate::compile(file) {
            Ok((bytecode, constants)) => self.compile_pather(bytecode, constants),
            Err(e) => return Err(e),
        }

        Ok(())
    }

    fn compile_pather(&mut self, bytecode: Bytecode, constants: Vec<Constant>) {
        let len = self.constants.len();
        let bt_len = self.compiled_opcodes();
        constants.into_iter().for_each(|it| {
            self.constants.push(match it {
                Constant::Fun(f) => {
                    let body = patch_bytecode(
                        len,
                        bt_len,
                        match f.body.get() {
                            Either::Left(body) => body,
                            _ => unreachable!(),
                        },
                    );
                    Constant::Fun(GcRef::new(Fun {
                        arity: f.arity,
                        body: GcRef::new(Either::Left(body)),
                        args: f.args.clone(),
                    }))
                }
                other => other,
            })
        });
        let bytecode = patch_bytecode(len, bt_len, &bytecode);
        bytecode.into_iter().for_each(|it| self.emit_metadata(it));
    }

    fn compiled_opcodes(&self) -> usize {
        self.proxies.last().unwrap().opcodes.len()
    }

    fn variables(&mut self) -> &mut HashMap<Symbol, usize> {
        &mut self.proxies.last_mut().unwrap().vars
    }

    fn emit(&mut self, intr: OpCode) {
        let op = OpCodeMetadata {
            line: self.current.line,
            column: self.current.column,
            opcode: intr,
        };
        self.emit_metadata(op)
    }

    fn emit_metadata(&mut self, op: OpCodeMetadata) {
        let proxy = self.proxies.last_mut().unwrap();
        proxy.opcodes.push(op);
    }

    fn emit_patch(&mut self, intr: OpCode, idx: usize) {
        let proxy = self.proxies.last_mut().unwrap();
        proxy.opcodes[idx].opcode = intr;
    }

    fn emit_const_push(&mut self, constant: Constant) {
        if let Some(idx) = self.constants.iter().position(|c| c == &constant) {
            self.emit(OpCode::Push(idx))
        } else {
            self.emit(OpCode::Push(self.constants.len()));
            self.constants.push(constant)
        }
    }

    fn emit_load(&mut self, name: Symbol) {
        if let Some(index) = self.variables().get(&name).copied() {
            self.emit(OpCode::Load(index))
        } else {
            self.emit(OpCode::Loag(name))
        }
    }

    fn emit_save(&mut self, name: Symbol) {
        let len = self.variables().len();
        self.variables().insert(name, len);
        self.emit(OpCode::Save(len));
    }

    fn emit_drop(&mut self, name: Symbol) {
        if let Some(index) = self.proxies.last().unwrap().vars.get(&name).copied() {
            self.emit(OpCode::Drop(index))
        } else {
            unreachable!()
        }
    }

    fn next(&mut self) -> ParseResult {
        let tk = self.lexer.next();
        self.current = tk.unwrap_or(Ok(Token {
            line: 0,
            column: 0,
            token: Tkt::Eof,
        }))?;

        Ok(())
    }

    fn assert(&mut self, token: &[Tkt], err: impl Into<String>) -> ParseResult {
        if !token.contains(&self.current.token) {
            self.throw(err)
        } else {
            Ok(())
        }
    }

    fn consume(&mut self, token: &[Tkt], err: impl Into<String>) -> ParseResult {
        self.assert(token, err)?;
        self.next()?;

        Ok(())
    }

    fn throw(&self, err: impl Into<String>) -> ParseResult {
        ParseError::throw(self.current.line, self.current.column, err.into())
    }

    fn new_proxy(&mut self) {
        self.proxies.push(Proxy::default())
    }

    fn expression(&mut self) -> ParseResult {
        loop {
            match self.current.token {
                Tkt::If => self.condition(),
                Tkt::Let => self.let_(),
                Tkt::Fn => self.fn_(),
                Tkt::Become => self.become_(),
                Tkt::Loop => self.loop_(),
                _ => self.pipe(),
            }?;

            if self.current.token != Tkt::Seq {
                break;
            }
            self.emit(OpCode::Pop);
            self.next()?;
        }

        Ok(())
    }

    fn loop_(&mut self) -> ParseResult {
        self.next()?;
        let offset = self.compiled_opcodes();
        self.expression()?;
        self.emit(OpCode::Pop);
        self.emit(OpCode::Jmp(offset));

        Ok(())
    }

    fn function(&mut self) -> ParseResult {
        let mut arity = 0;
        self.new_proxy();

        while matches!(self.current.token, Tkt::Name(_)) {
            let id = match take(&mut self.current.token) {
                Tkt::Name(id) => id,
                _ => unreachable!(),
            };

            self.emit_save(Symbol::new(id));
            arity += 1;
            self.next()?;
        }

        self.consume(
            &[Tkt::Assign],
            format!(
                "Expected `=` after argument, found `{}`",
                self.current.token
            ),
        )?;

        self.expression()?;
        let body = self.proxies.pop().unwrap();

        self.emit_const_push(Constant::Fun(GcRef::new(Fun {
            body: GcRef::new(Either::Left(body.opcodes)),
            arity,
            args: stackvec![],
        })));

        Ok(())
    }

    fn become_(&mut self) -> ParseResult {
        assert_eq!(self.current.token, Tkt::Become); // security check
        self.next()?;
        self.call()?;
        let proxy = &mut self.proxies.last_mut().unwrap();
        match proxy.opcodes.pop() {
            Some(OpCodeMetadata {
                opcode: OpCode::Call(arity),
                line,
                column,
            }) => self.emit_metadata(OpCodeMetadata {
                line,
                column,
                opcode: OpCode::TCall(arity),
            }),
            _ => self.throw("Expected call after become")?,
        }
        self.next()?;
        Ok(())
    }

    fn fn_(&mut self) -> ParseResult {
        assert_eq!(self.current.token, Tkt::Fn); // security check
        self.next()?;
        self.function()
    }

    fn if_elif(&mut self) -> Result<usize, ParseError> {
        self.next()?; // skips the if token

        self.expression()?; // compiles the condition
        self.consume(
            &[Tkt::Then],
            format!(
                "expected `then` after condition, found `{}`",
                &self.current.token
            ),
        )?; // checks for then

        let then_jump_ip = self.compiled_opcodes();
        self.emit(OpCode::Jmf(0));

        self.expression()?; // compiles the if branch

        Ok(then_jump_ip)
    }

    fn condition(&mut self) -> ParseResult {
        assert_eq!(self.current.token, Tkt::If); // security check

        self.emit(OpCode::Nsc); // creates a new scope

        let mut patch_stack = vec![];

        while matches!(self.current.token, Tkt::If) {
            let then_jump_ip = self.if_elif()?;
            self.emit_patch(OpCode::Jmf(self.compiled_opcodes() + 1), then_jump_ip);

            patch_stack.push(self.compiled_opcodes());
            self.emit(OpCode::Jmp(0));
        }

        self.consume(
            &[Tkt::Else],
            format!("Expected `else` after if, found `{}`", self.current.token),
        )?;
        self.expression()?; // compiles the else branch

        let compiled_opcodes = self.compiled_opcodes();
        let jmp = OpCode::Jmp(compiled_opcodes);

        patch_stack
            .into_iter()
            .for_each(|it| self.emit_patch(jmp, it));

        self.emit(OpCode::Esc); // End the new scope

        Ok(())
    }

    fn let_(&mut self) -> ParseResult {
        assert_eq!(self.current.token, Tkt::Let); // security check
        self.next()?; // skips the let token

        let name = match take(&mut self.current.token) {
            Tkt::Name(v) => Symbol::new(v),
            o => return self.throw(format!("Expected variable name after `let`, found `{}`", o)),
        };
        self.next()?;

        if matches!(self.current.token, Tkt::Name(_)) {
            self.function()?;
        } else {
            self.consume(
                &[Tkt::Assign],
                format!("Expected `=` after name, found `{}`", self.current.token),
            )?;
            self.expression()?;
        }

        self.emit_save(name);

        self.consume(
            &[Tkt::In],
            format!(
                "Expected `in` after let expression, found {}",
                self.current.token
            ),
        )?;
        self.expression()?;

        self.emit_drop(name);

        Ok(())
    }

    fn pipe(&mut self) -> ParseResult {
        self.or()?;

        while let Tkt::Pipe = self.current.token {
            self.next()?;
            self.or()?;
            self.emit(OpCode::Call(1))
        }

        Ok(())
    }

    fn or(&mut self) -> ParseResult {
        self.and()?;

        while let Tkt::Or = self.current.token {
            self.next()?;
            self.emit(OpCode::Dup);
            self.emit(OpCode::Not);

            let ip = self.compiled_opcodes();
            self.emit(OpCode::Jmf(0));

            self.emit(OpCode::Pop);
            self.and()?;

            self.emit_patch(OpCode::Jmf(self.compiled_opcodes()), ip);
        }

        Ok(())
    }

    fn and(&mut self) -> ParseResult {
        self.equality()?;

        while let Tkt::And = self.current.token {
            self.next()?;
            self.emit(OpCode::Dup);

            let ip = self.compiled_opcodes();
            self.emit(OpCode::Jmf(0));

            self.emit(OpCode::Pop);
            self.equality()?;

            self.emit_patch(OpCode::Jmf(self.compiled_opcodes()), ip);
        }

        Ok(())
    }

    fn equality(&mut self) -> ParseResult {
        self.cmp()?;

        while let Tkt::Eq = self.current.token {
            let operator = match self.current.token {
                Tkt::Eq => OpCode::Eq,
                _ => unreachable!(),
            };
            self.next()?;
            self.equality()?;
            self.emit(operator);
        }

        Ok(())
    }
    fn cmp(&mut self) -> ParseResult {
        self.cons()?;
        while let Tkt::LessEq | Tkt::Less | Tkt::Greater | Tkt::GreaterEq = self.current.token {
            let operator = match self.current.token {
                Tkt::LessEq => OpCode::LessEq,
                Tkt::Greater => OpCode::Greater,
                Tkt::Less => OpCode::Less,
                Tkt::GreaterEq => OpCode::GreaterEq,
                _ => unreachable!(),
            };
            self.next()?;
            self.cons()?;
            self.emit(operator);
        }
        Ok(())
    }

    fn cons(&mut self) -> ParseResult {
        self.bitwise()?;

        while Tkt::Cons == self.current.token {
            self.next()?;
            self.cons()?;
            self.emit(OpCode::Rev);
            self.emit(OpCode::Prep);
        }

        Ok(())
    }

    fn bitwise(&mut self) -> ParseResult {
        self.term()?; // expands to a unary rule

        while let Tkt::BitAnd | Tkt::BitOr | Tkt::Shr | Tkt::Shl | Tkt::BitXor = self.current.token
        {
            let operator = match self.current.token {
                Tkt::BitAnd => OpCode::BitAnd,
                Tkt::BitOr => OpCode::BitOr,
                Tkt::BitXor => OpCode::Xor,
                Tkt::Shr => OpCode::Shr,
                Tkt::Shl => OpCode::Shl,
                _ => unreachable!(),
            };
            self.next()?;
            self.bitwise()?;
            self.emit(operator);
        }

        Ok(())
    }

    fn term(&mut self) -> ParseResult {
        self.fact()?; // expands to a unary rule

        while let Tkt::Add | Tkt::Sub = self.current.token {
            let operator = match self.current.token {
                Tkt::Add => OpCode::Add,
                Tkt::Sub => OpCode::Sub,
                _ => unreachable!(),
            };
            self.next()?;
            self.fact()?;
            self.emit(operator);
        }

        Ok(())
    }

    fn fact(&mut self) -> ParseResult {
        self.unary()?; // expands to a unary rule

        while let Tkt::Mul | Tkt::Div = self.current.token {
            let operator = match self.current.token {
                Tkt::Mul => OpCode::Mul,
                Tkt::Div => OpCode::Div,
                _ => unreachable!(),
            };
            self.next()?;
            self.unary()?;
            self.emit(operator);
        }

        Ok(())
    }

    fn unary(&mut self) -> ParseResult {
        use OpCode::*;

        if matches!(self.current.token, Tkt::Sub | Tkt::Not | Tkt::Len) {
            let operator = match self.current.token {
                Tkt::Sub => Neg,
                Tkt::Not => Not,
                Tkt::Len => Len,
                _ => unreachable!(),
            };
            self.next()?;
            self.unary()?; // emits the expression to be applied
            self.emit(operator)
        } else {
            self.index()?;
            self.next()?;
        }

        Ok(())
    }

    fn call_args(&mut self, arity: &mut usize) -> ParseResult {
        self.next()?;
        self.next()?;

        while self.current.token != Tkt::Rparen {
            self.expression()?;
            self.emit(OpCode::Rev);
            *arity += 1;
            match &self.current.token {
                Tkt::Rparen => break,
                Tkt::Colon => self.next()?,
                other => self.throw(format!(
                    "Expected `,`, `)` or other token, found `{}`",
                    other
                ))?,
            }
        }

        Ok(())
    }

    fn index(&mut self) -> ParseResult {
        self.call()?; // Emits the expression to be indexed

        while matches!(
            self.lexer.peek().unwrap().as_ref().map(|c| &c.token),
            Ok(Tkt::Lbrack)
        ) {
            self.next()?;
            self.next()?;
            self.expression()?; // emits the index to be acessed
            self.emit(OpCode::Index);
            self.assert(
                &[Tkt::Rbrack],
                format!("Expected `]` after index, found {}", self.current.token),
            )?;
        }
        Ok(())
    }

    fn call(&mut self) -> ParseResult {
        self.primary()?; // compiles the called expresion

        let mut arity = 0;
        let mut is_call = false;

        while matches!(
            self.lexer.peek().unwrap().as_ref().map(|c| &c.token),
            Ok(Tkt::Lparen)
        ) {
            self.call_args(&mut arity)?;
            is_call = true;
        }

        if is_call {
            self.emit(OpCode::Call(arity));
        }

        Ok(())
    }

    fn list(&mut self) -> ParseResult {
        let mut len = 0;
        loop {
            if matches!(self.current.token, Tkt::Rbrack) {
                break;
            }
            self.next()?;
            if matches!(self.current.token, Tkt::Rbrack) {
                break;
            }

            self.expression()?; // compiles the argument
            len += 1;

            if !matches!(&self.current.token, Tkt::Colon | Tkt::Rbrack) {
                self.throw(format!(
                    "Expected `,`, `]` or other token, found `{}`",
                    &self.current.token
                ))?;
            }
        }

        self.emit_const_push(Constant::List(GcRef::new(List::new())));

        while len > 0 {
            self.emit(OpCode::Rev);
            self.emit(OpCode::Prep);
            len -= 1;
        }

        Ok(())
    }

    fn table(&mut self) -> ParseResult {
        self.emit_const_push(Constant::Table(GcRef::new(Table::new())));

        loop {
            if matches!(self.current.token, Tkt::Rbrace) {
                break;
            }
            self.next()?;
            if matches!(self.current.token, Tkt::Rbrace) {
                break;
            }

            let sym = match self.current.token {
                Tkt::Sym(s) => s,
                ref other => {
                    return self.throw(format!("Expected symbol to use as key, found `{}`", other))
                }
            };

            self.next()?;
            self.consume(
                &[Tkt::Assign],
                format!("Expected `=` after key, found {}", &self.current.token),
            )?;

            self.expression()?; // compiles the argument
            self.emit(OpCode::Insert(sym));

            if !matches!(&self.current.token, Tkt::Colon | Tkt::Rbrace) {
                self.throw(format!(
                    "Expected `,`, `]` or other token, found `{}`",
                    &self.current.token
                ))?;
            }
        }

        Ok(())
    }

    fn block(&mut self) -> ParseResult {
        assert_eq!(self.current.token, Tkt::Lparen);

        self.next()?;
        self.expression()?;
        self.assert(
            &[Tkt::Rparen],
            format!(
                "expected `)` to close the block, found `{}`",
                self.current.token
            ),
        )
    }

    fn primary(&mut self) -> ParseResult {
        macro_rules! push {
            ($type: expr) => {{
                self.emit_const_push($type);
            }};
        }

        use Constant::*;

        match take(&mut self.current.token) {
            Tkt::Num(n) => push!(Num(n)),
            Tkt::Str(str) => push!(Str(GcRef::new(str))),
            Tkt::Sym(sym) => push!(Sym(sym)), // don't allow for duplicated symbols
            Tkt::True => push!(Bool(true)),
            Tkt::False => push!(Bool(false)),
            Tkt::Name(v) => {
                let v = Symbol::new(v);
                self.emit_load(v);
            }
            Tkt::Nil => push!(Nil),
            Tkt::Lbrack => self.list()?,
            Tkt::Lbrace => self.table()?,
            Tkt::Lparen => {
                self.current.token = Tkt::Lparen; // `(` is needed for self.block() to work correctly
                self.block()?;
            }
            tk => self.throw(format!("expected expression, found `{}`", tk))?,
        }

        Ok(())
    }
}
