use either::Either;
use libc;
use globset::{self, GlobBuilder, GlobSetBuilder, Glob};
use walkdir::WalkDir;

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::{OsString, OsStr};
use std::io::Write;
use std::iter::FromIterator;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::process::{Child, Stdio};
use std::rc::Rc;

use super::env::Environment;
use ::UtilSetup;

type ExitCode = libc::c_int;

#[derive(Debug)]
pub struct Script {
    commands: Vec<CompleteCommand>
}

impl Script {
    pub fn new(commands: Vec<CompleteCommand>) -> Self {
        Self {
            commands: commands
        }
    }
}

#[derive(Debug)]
pub struct CompleteCommand {
    lists: Vec<Vec<Vec<AndOr>>>,
}

impl CompleteCommand {
    pub fn new(lists: Vec<Vec<Vec<AndOr>>>) -> Self {
        Self {
            lists: lists
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TODO: set envs and stuff
        let mut code = 0;
        for list in &self.lists {
            code = exec_list(setup, env, list);
        }
        code
    }
}

pub type VarName = OsString;

//pub type Word = OsString;
pub type Name = OsString;
pub type CommandName = Word;

// split into the two types to avoid allocating an extra vector for every word
// XXX: maybe store ParamExpand (at least) too?
// FIXME: needs to store more than just text (text works fine without globbing, but once you add that single quotes no longer
//        function the same as just straight up text (single quotes won't evaluate the glob, whereas the glob needs to be
//        evaluated if given like `echo *`))
#[derive(Debug)]
pub enum Word {
    Text(OsString),
    Complex(Vec<WordPart>),
}

impl Word {
    pub fn eval<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> OsString
    where
        S: UtilSetup,
    {
        use self::Word::*;

        match self {
            Text(ref s) => s.clone(),
            Complex(ref parts) => {
                parts.iter().fold(OsString::new(), |mut acc, item| {
                    acc.push(&item.eval(setup, env));
                    acc
                })
            }
        }
    }

    pub fn matches_glob<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>, value: &OsStr) -> bool
    where
        S: UtilSetup,
    {
        let text = self.eval(setup, env);

        // FIXME: what to do here, error out?
        let glob = match self.create_glob(&text) {
            Ok(m) => m,
            _ => return false
        };
        let matcher = glob.compile_matcher();

        if matcher.is_match(value) {
            true
        } else {
            false
        }
    }

    // NOTE: globset seems to implement filename{a,b} syntax, which is not valid for posix shell technically
    // NOTE: * shouldn't list hidden files
    pub fn eval_glob_fs<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> WordEval
    where
        S: UtilSetup,
    {
        let text = self.eval(setup, env);

        match self.create_glob(&text).and_then(|glob| GlobSetBuilder::new().add(glob).build()) {
            Ok(set) => {
                // FIXME: this should use the current_dir in setup (or whatever it has been changed
                //        to during the course of the program's lifetime)
                /*let mut res = GlobWalker::from_globset(set)/*.base_dir()*/.into_iter().fold(vec![], |mut acc, entry| {
                    // FIXME: not sure what to do on entry failure (do we bail or just report an error?)
                    if let Ok(entry) = entry {
                        acc.push(entry.path().as_os_str().to_owned());
                    }
                    acc
                });*/
                // TODO: need to create custom walker first
                let res = vec![];
                if res.is_empty() {
                    WordEval::Text(text)
                } else {
                    WordEval::Globbed(res)
                }
            }
            Err(_) => {
                // in this case, we just assume that the "glob" is actual meant to be a literal
                WordEval::Text(text)
            }
        }
    }

    fn create_glob(&self, text: &OsStr) -> Result<Glob, globset::Error> {
        // sadly, we need to convert any non-utf8 values to hex escapes to compile the regex, which
        // wastes some time
        let glob_str = self.escape_glob(&text);

        GlobBuilder::new(&glob_str).literal_separator(true).build()
    }

    // NOTE: i think we may need to escape in any case (e.g. if * is in single quotes like abc'*', we need
    //       to match literally abc* not abcdef (for example)).  we'll have to escape the */?/whatever is
    //       used in patterns if they are in single quotes
    fn escape_glob<'a>(&self, text: &'a OsStr) -> Cow<'a, str> {
        // this is optimized for utf8 (try to convert and then go back through the string and
        // escape incorrect values on failure), as i imagine most given strings will be valid utf8
        match text.to_str() {
            Some(s) => Cow::from(s),
            None => {
                // the text is invalid utf8, so convert manually by escaping
                Cow::from(text.as_bytes().iter().fold(String::new(), |mut acc, &byte| {
                    let ch: char = byte.into();
                    if ch.is_ascii() {
                        acc.push(ch);
                    } else {
                        acc.extend(ch.escape_default());
                    }
                    acc
                }))
            }
        }
    }
}

#[derive(Debug)]
pub enum WordPart {
    Text(OsString),
    ParamExpand,        // TODO
    CommandSubst,       // TODO
}

impl WordPart {
    pub fn eval<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> OsString
    where
        S: UtilSetup,
    {
        use self::WordPart::*;

        match self {
            Text(ref s) => s.clone(),
            _ => unimplemented!()
        }
    }

    pub fn is_text(&self) -> bool {
        match self {
            WordPart::Text(_) => true,
            _ => false
        }
    }
}

#[derive(Debug)]
pub enum WordEval {
    Text(OsString),
    Globbed(Vec<OsString>),
}

#[derive(Debug)]
pub struct AndOr {
    pipeline: Pipeline,
    separator: SepKind,
}

impl AndOr {
    pub fn new(pipeline: Pipeline, sep: SepKind) -> Self {
        Self {
            pipeline: pipeline,
            separator: sep,
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>, prev_res: ExitCode) -> ExitCode
    where
        S: UtilSetup,
    {
        match (self.separator, prev_res) {
            (SepKind::First, _) | (SepKind::And, 0) => self.pipeline.execute(setup, env),
            (SepKind::Or, res) if res != 0 => self.pipeline.execute(setup, env),
            (_, prev_res) => prev_res
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SepKind {
    First,
    And,
    Or,
}

#[derive(Debug)]
pub struct VarAssign {
    pub varname: VarName,
    pub value: ()//Expr,
}

#[derive(Debug)]
pub struct Pipeline {
    commands: Vec<Command>,
    pub bang: bool,
}

impl Pipeline {
    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // FIXME: what this should actually do is set up commands where output of a commands is piped to the input of the next command and then start them all
        let mut code = 0;
        for cmd in &self.commands {
            code = cmd.execute(setup, env);
        }

        if self.bang {
            (code == 0) as ExitCode
        } else {
            code
        }
    }
}

impl FromIterator<Command> for Pipeline {
    fn from_iter<I: IntoIterator<Item = Command>>(iter: I) -> Self {
        Pipeline {
            commands: iter.into_iter().collect(),
            bang: false,
        }
    }
}

#[derive(Debug)]
pub struct Command {
    inner: CommandInner,
    pub redirect_list: Option<Vec<IoRedirect>>,
}

impl Command {
    pub fn with_inner(inner: CommandInner) -> Self {
        Self {
            inner: inner,
            redirect_list: None
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // FIXME: what this should actually do is setup a command with stdin/stdout/stderr redirected to whatever is specified in redirect_list and return it
        self.inner.execute(setup, env)
    }
}

#[derive(Debug)]
pub enum CommandInner {
    If(Box<IfClause>),
    While(Box<WhileClause>),
    For(Box<ForClause>),
    Case(Box<CaseClause>),
    FunctionDef(Box<FunctionDef>),
    AndOr(Vec<Vec<AndOr>>),
    Simple(SimpleCommand),
}

impl CommandInner {
    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        use self::CommandInner::*;

        // FIXME: needs to set up redirects somehow
        match self {
            If(ref clause) => clause.execute(setup, env),
            While(ref clause) => clause.execute(setup, env),
            For(ref clause) => clause.execute(setup, env),
            Case(ref clause) => clause.execute(setup, env),
            FunctionDef(ref def) => def.execute(setup, env),
            AndOr(ref and_ors) => exec_list(setup, env, and_ors),
            Simple(ref cmd) => cmd.execute(setup, env),
        }
    }
}

impl From<IfClause> for Command {
    fn from(value: IfClause) -> Self {
        Command::with_inner(CommandInner::If(Box::new(value)))
    }
}

impl From<WhileClause> for Command {
    fn from(value: WhileClause) -> Self {
        Command::with_inner(CommandInner::While(Box::new(value)))
    }
}

impl From<ForClause> for Command {
    fn from(value: ForClause) -> Self {
        Command::with_inner(CommandInner::For(Box::new(value)))
    }
}

impl From<CaseClause> for Command {
    fn from(value: CaseClause) -> Self {
        Command::with_inner(CommandInner::Case(Box::new(value)))
    }
}

impl From<FunctionDef> for Command {
    fn from(value: FunctionDef) -> Self {
        Command::with_inner(CommandInner::FunctionDef(Box::new(value)))
    }
}

impl From<SimpleCommand> for Command {
    fn from(value: SimpleCommand) -> Self {
        Command::with_inner(CommandInner::Simple(value))
    }
}

#[derive(Debug)]
pub struct IfClause {
    cond: Command,
    body: Command,
    else_stmt: Option<ElseClause>,
}

impl IfClause {
    pub fn new(cond: Command, body: Command, else_stmt: Option<ElseClause>) -> Self {
        Self {
            cond: cond,
            body: body,
            else_stmt: else_stmt,
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TODO: redirects
        if self.cond.execute(setup, env) == 0 {
            self.body.execute(setup, env)
        } else if let Some(ref clause) = self.else_stmt {
            clause.execute(setup, env)
        } else {
            0
        }
    }
}

#[derive(Debug)]
pub struct ElseClause {
    cond: Option<Command>,
    body: Command,
    else_stmt: Option<Box<ElseClause>>,
}

impl ElseClause {
    pub fn new(cond: Option<Command>, body: Command, else_stmt: Option<Box<Self>>) -> Self {
        Self {
            cond: cond,
            body: body,
            else_stmt: else_stmt,
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TOD: redirects
        if let Some(ref cmd) = self.cond {
            if cmd.execute(setup, env) != 0 {
                return match self.else_stmt {
                    Some(ref clause) => clause.execute(setup, env),
                    None => 0
                };
            }
        }
        self.body.execute(setup, env)
    }
}

#[derive(Debug)]
pub struct WhileClause {
    cond: Command,
    desired: bool,
    body: Command,
}

impl WhileClause {
    pub fn new(cond: Command, desired: bool, body: Command) -> Self {
        Self {
            cond: cond,
            desired: desired,
            body: body
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TODO: redirects
        let mut code = 0;
        while (self.cond.execute(setup, env) == 0) == self.desired {
            code = self.body.execute(setup, env);
        }
        code
    }
}

#[derive(Debug)]
pub struct ForClause {
    name: Name,
    words: Option<Vec<Word>>,
    body: Command,
}

impl ForClause {
    pub fn new(name: Name, words: Option<Vec<Word>>, body: Command) -> Self {
        Self {
            name: name,
            words: words,
            body: body
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TODO: redirects
        // TODO: when self.words is None it should act as if it were the value in $@ (retrieve from env)
        let words = match self.words {
            Some(ref words) => &words[..],
            None => unimplemented!()
        };

        let mut code = 0;
        for word in words {
            let value = word.eval(setup, env);
            env.set_var(&self.name, value);
            code = self.body.execute(setup, env);
        }
        code
    }
}

#[derive(Debug)]
pub struct CaseClause {
    word: Word,
    case_list: CaseList
}

impl CaseClause {
    pub fn new(word: Word, case_list: CaseList) -> Self {
        Self {
            word: word,
            case_list: case_list
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TODO: redirects
        self.case_list.execute(setup, env, &self.word)
    }
}

#[derive(Debug)]
pub struct CaseList {
    items: Vec<CaseItem>,
}

impl CaseList {
    pub fn new(items: Vec<CaseItem>) -> Self {
        Self {
            items: items
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>, word: &'a Word) -> ExitCode
    where
        S: UtilSetup,
    {
        let word_str = word.eval(setup, env);
        for item in &self.items {
            if let Some(code) = item.execute(setup, env, &word_str) {
                return code;
            }
        }
        0
    }
}

#[derive(Debug)]
pub struct CaseItem {
    pattern: Pattern,
    actions: Option<Command>
}

impl CaseItem {
    pub fn new(pattern: Pattern, actions: Option<Command>) -> Self {
        Self {
            pattern: pattern,
            actions: actions
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>, word: &OsStr) -> Option<ExitCode>
    where
        S: UtilSetup,
    {
        if self.pattern.matches(setup, env, word) {
            Some(if let Some(ref cmd) = self.actions {
                cmd.execute(setup, env)
            } else {
                0
            })
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct Pattern {
    items: Vec<Word>,
}

impl Pattern {
    pub fn new(items: Vec<Word>) -> Self {
        Self {
            items: items
        }
    }

    pub fn matches<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>, word: &OsStr) -> bool
    where
        S: UtilSetup,
    {
        for item in &self.items {
            if item.matches_glob(setup, env, word) {
                return true;
            }
        }
        false
    }
}

#[derive(Debug)]
pub struct FunctionDef {
    name: Name,
    body: FunctionBody
}

impl FunctionDef {
    pub fn new(name: Name, body: FunctionBody) -> Self {
        Self {
            name: name,
            body: body
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        env.set_func(&self.name, &self.body);
        // XXX: is there actually a way for this to not be 0?  spec says non-zero on failure, so is this just in parsing?
        0
    }
}

#[derive(Debug)]
pub struct FunctionBody {
    command: Command,
    redirect_list: Option<Vec<IoRedirect>>,
}

impl FunctionBody {
    pub fn new(command: Command, redirect_list: Option<Vec<IoRedirect>>) -> Self {
        Self {
            command: command,
            redirect_list: redirect_list
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        // TODO: redirects
        self.command.execute(setup, env)
    }
}

#[derive(Debug)]
pub struct SimpleCommand {
    pre_actions: Option<Vec<PreAction>>,
    post_actions: Option<Vec<PostAction>>,
    name: Option<Word>,
}

impl SimpleCommand {
    pub fn new(name: Option<Word>, pre_actions: Option<Vec<PreAction>>, post_actions: Option<Vec<PostAction>>) -> Self {
        Self {
            name: name,
            pre_actions: pre_actions,
            post_actions: post_actions
        }
    }

    pub fn execute<'a, S>(&'a self, setup: &mut S, env: &mut Environment<'a>) -> ExitCode
    where
        S: UtilSetup,
    {
        use std::process::Command as RealCommand;

        if let Some(ref name) = self.name {
            let mut cmd = RealCommand::new(name.eval(setup, env));
            //println!("{}", name.to_string_lossy());
            cmd.env_clear();

            cmd.envs(env.iter());
            // TODO: redirects and pre/post actions (other than args)
            if let Some(ref actions) = self.post_actions {
                for act in actions.iter() {
                    match act {
                        Either::Left(ref redirect) => {
                            // FIXME: only works after spawning process i guess
                            redirect.setup(&mut cmd);
                        }
                        Either::Right(ref word) => {
                            match word.eval_glob_fs(setup, env) {
                                WordEval::Text(text) => cmd.arg(text),
                                WordEval::Globbed(glob) => cmd.args(glob.iter()),
                            };
                        }
                    }
                }
            }

            // TODO: this probably shouldn't wait but not sure what to do exactly
            // FIXME: don't unwrap
            let mut child = cmd.spawn().unwrap();
            if let Some(ref actions) = self.post_actions {
                // FIXME: don't unwrap
                for act in actions.iter() {
                    match act {
                        Either::Left(ref redirect) => {
                            // FIXME: only works after spawning process i guess
                            redirect.handle_child(&mut child);
                        }
                        _ => {}
                        //Either::Right(ref word) => cmd.arg(word)
                    }
                }
            }
            return child.wait().unwrap().code().unwrap();
        }

        0
    }
}

pub type PreAction = Either<IoRedirect, VarAssign>;
pub type PostAction = Either<IoRedirect, Word>;

#[derive(Debug)]
pub enum IoRedirect {
    File(Option<RawFd>, IoRedirectFile),
    Heredoc(Option<RawFd>, Rc<RefCell<HereDoc>>)
}

impl IoRedirect {
    pub fn setup(&self, cmd: &mut ::std::process::Command) {
        use self::IoRedirect::*;

        // TODO: both file and check for fds
        match self {
            Heredoc(_, ref doc) => {
                cmd.stdin(Stdio::piped());
            }
            _ => {}
        }
    }

    // FIXME: this can prob error
    pub fn handle_child(&self, child: &mut Child) {
        use self::IoRedirect::*;

        // TODO: if we need to handle something other than heredocs do so
        match self {
            Heredoc(_, ref doc) => {
                // FIXME: don't unwrap
                child.stdin.as_mut().unwrap().write_all(&doc.borrow().data).unwrap();
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
pub struct IoRedirectFile {
    filename: Word,
    kind: IoRedirectKind
}

impl IoRedirectFile {
    pub fn new(kind: IoRedirectKind, filename: Word) -> Self {
        Self {
            kind: kind,
            filename: filename,
        }
    }
}

#[derive(Debug)]
pub enum IoRedirectKind {
    Input,
    DupInput,
    Output,
    DupOutput,
    ReadWrite,
    Append,
    Clobber
}

#[derive(Debug)]
pub struct HereDoc {
    pub data: Vec<u8>,
}

impl HereDoc {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: data
        }
    }
}

impl Default for HereDoc {
    fn default() -> Self {
        Self::new(vec![])
    }
}

// TODO: figure out how to get this to work (pass the child up the call chain?)
#[derive(Debug)]
pub struct CommandSubst {
    command: Command,
}

impl CommandSubst {
    pub fn new(cmd: Command) -> Self {
        Self {
            command: cmd,
        }
    }

    pub fn eval<S>(&self, setup: &mut S, env: &mut Environment) -> OsString
    where
        S: UtilSetup,
    {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct DoubleQuote {
    items: Vec<Quotable>
}

impl DoubleQuote {
    pub fn eval<S>(&self, setup: &mut S, env: &mut Environment) -> OsString
    where
        S: UtilSetup,
    {
        self.items.iter().fold(OsString::new(), |mut acc, item| {
            acc.push(&item.eval(setup, env));
            acc
        })
    }
}

#[derive(Debug)]
pub enum Quotable {
    CommandSubst(CommandSubst),
    ArithExpr,      // TODO: actually add support for this
    ParamExpand,    // TODO: add support
    Text(OsString),
}

impl Quotable {
    pub fn eval<S>(&self, setup: &mut S, env: &mut Environment) -> OsString
    where
        S: UtilSetup,
    {
        use self::Quotable::*;

        match self {
            CommandSubst(ref sub) => sub.eval(setup, env),
            Text(ref s) => s.clone(),
            _ => unimplemented!()
        }
    }
}

fn exec_andor_chain<'a, S>(setup: &mut S, env: &mut Environment<'a>, chain: &'a [AndOr]) -> ExitCode
where
    S: UtilSetup,
{
    let mut code = 0;
    for and_or in chain {
        code = and_or.execute(setup, env, code);
    }
    code
}

fn exec_list<'a, S>(setup: &mut S, env: &mut Environment<'a>, list: &'a [Vec<AndOr>]) -> ExitCode
where
    S: UtilSetup,
{
    let mut code = 0;
    for chain in list {
        code = exec_andor_chain(setup, env, chain);
    }
    code
}
