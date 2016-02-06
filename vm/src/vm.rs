use std::cell::{Cell, RefCell, Ref, RefMut};
use std::error::Error as StdError;
use std::fmt;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::cmp::Ordering;
use std::ops::Deref;
use std::string::String as StdString;
#[cfg(feature = "parser")]
use base::ast;
use base::ast::{Typed, ASTType, DisplayEnv};
use base::symbol::{Name, NameBuf, Symbol, Symbols};
use base::types::{KindEnv, TypeEnv, TcType, RcKind};
use base::macros::MacroEnv;
use base::types;
use base::types::Type;
use types::*;
use base::fixed::{FixedMap, FixedVec};
use interner::{Interner, InternedStr};
use gc::{Gc, GcPtr, Traverseable, DataDef, Move, WriteOnly};
use array::{Array, Str};
use compiler::{CompiledFunction, Variable, CompilerEnv};
use api::{Getable, Pushable, VMType};
use lazy::Lazy;

use self::Named::*;

use self::Value::{Int, Float, String, Data, Function, PartialApplication, Closure, Userdata};


use stack::{Stack, StackFrame};

#[derive(Copy, Clone, Debug)]
pub struct Userdata_ {
    pub data: GcPtr<Box<Any>>,
}

impl Userdata_ {
    pub fn new<T: Any>(vm: &VM, v: T) -> Userdata_ {
        let v: Box<Any> = Box::new(v);
        Userdata_ { data: vm.gc.borrow_mut().alloc(Move(v)) }
    }
    fn ptr(&self) -> *const () {
        let p: *const _ = &*self.data;
        p as *const ()
    }
}
impl PartialEq for Userdata_ {
    fn eq(&self, o: &Userdata_) -> bool {
        self.ptr() == o.ptr()
    }
}

#[derive(Debug)]
pub struct ClosureData<'a> {
    pub function: GcPtr<BytecodeFunction>,
    pub upvars: Array<Cell<Value<'a>>>,
}

impl<'a> PartialEq for ClosureData<'a> {
    fn eq(&self, _: &ClosureData<'a>) -> bool {
        false
    }
}

impl<'a> Traverseable for ClosureData<'a> {
    fn traverse(&self, gc: &mut Gc) {
        self.function.traverse(gc);
        self.upvars.traverse(gc);
    }
}

struct ClosureDataDef<'a: 'b, 'b>(GcPtr<BytecodeFunction>, &'b [Value<'a>]);
impl<'a, 'b> Traverseable for ClosureDataDef<'a, 'b> {
    fn traverse(&self, gc: &mut Gc) {
        self.0.traverse(gc);
        self.1.traverse(gc);
    }
}

unsafe impl<'a: 'b, 'b> DataDef for ClosureDataDef<'a, 'b> {
    type Value = ClosureData<'a>;
    fn size(&self) -> usize {
        use std::mem::size_of;
        size_of::<GcPtr<BytecodeFunction>>() + Array::<Cell<Value<'a>>>::size_of(self.1.len())
    }
    fn initialize<'w>(self, mut result: WriteOnly<'w, ClosureData<'a>>) -> &'w mut ClosureData<'a> {
        unsafe {
            let result = &mut *result.as_mut_ptr();
            result.function = self.0;
            result.upvars.initialize(self.1.iter().map(|v| Cell::new(v.clone())));
            result
        }
    }
}

#[derive(Debug)]
pub struct BytecodeFunction {
    name: Option<Symbol>,
    args: VMIndex,
    instructions: Vec<Instruction>,
    inner_functions: Vec<GcPtr<BytecodeFunction>>,
    strings: Vec<InternedStr>,
}

impl BytecodeFunction {
    pub fn empty() -> BytecodeFunction {
        BytecodeFunction {
            name: None,
            args: 0,
            instructions: Vec::new(),
            inner_functions: Vec::new(),
            strings: Vec::new(),
        }
    }

    pub fn new(gc: &mut Gc, f: CompiledFunction) -> GcPtr<BytecodeFunction> {
        let CompiledFunction { id, args, instructions, inner_functions, strings, .. } = f;
        let fs = inner_functions.into_iter()
                                .map(|inner| BytecodeFunction::new(gc, inner))
                                .collect();
        gc.alloc(Move(BytecodeFunction {
            name: Some(id),
            args: args,
            instructions: instructions,
            inner_functions: fs,
            strings: strings,
        }))
    }
}

impl Traverseable for BytecodeFunction {
    fn traverse(&self, gc: &mut Gc) {
        self.inner_functions.traverse(gc);
    }
}

pub struct DataStruct<'a> {
    pub tag: VMTag,
    pub fields: Array<Cell<Value<'a>>>,
}

impl<'a> Traverseable for DataStruct<'a> {
    fn traverse(&self, gc: &mut Gc) {
        self.fields.traverse(gc);
    }
}

impl<'a> PartialEq for DataStruct<'a> {
    fn eq(&self, other: &DataStruct<'a>) -> bool {
        self.tag == other.tag && self.fields == other.fields
    }
}

pub type VMInt = isize;

#[derive(Copy, Clone, PartialEq)]
pub enum Value<'a> {
    Int(VMInt),
    Float(f64),
    String(GcPtr<Str>),
    Data(GcPtr<DataStruct<'a>>),
    Function(GcPtr<ExternFunction<'a>>),
    Closure(GcPtr<ClosureData<'a>>),
    PartialApplication(GcPtr<PartialApplicationData<'a>>),
    Userdata(Userdata_),
    Lazy(GcPtr<Lazy<'a, Value<'a>>>),
}

#[derive(Copy, Clone, Debug)]
pub enum Callable<'a> {
    Closure(GcPtr<ClosureData<'a>>),
    Extern(GcPtr<ExternFunction<'a>>),
}

impl<'a> Callable<'a> {
    pub fn name(&self) -> Option<Symbol> {
        match *self {
            Callable::Closure(ref closure) => closure.function.name,
            Callable::Extern(ref ext) => Some(ext.id),
        }
    }

    pub fn args(&self) -> VMIndex {
        match *self {
            Callable::Closure(ref closure) => closure.function.args,
            Callable::Extern(ref ext) => ext.args,
        }
    }
}

impl<'a> PartialEq for Callable<'a> {
    fn eq(&self, _: &Callable<'a>) -> bool {
        false
    }
}

impl<'a> Traverseable for Callable<'a> {
    fn traverse(&self, gc: &mut Gc) {
        match *self {
            Callable::Closure(ref closure) => closure.traverse(gc),
            Callable::Extern(_) => (),
        }
    }
}

#[derive(Debug)]
pub struct PartialApplicationData<'a> {
    function: Callable<'a>,
    arguments: Array<Cell<Value<'a>>>,
}

impl<'a> PartialEq for PartialApplicationData<'a> {
    fn eq(&self, _: &PartialApplicationData<'a>) -> bool {
        false
    }
}

impl<'a> Traverseable for PartialApplicationData<'a> {
    fn traverse(&self, gc: &mut Gc) {
        self.function.traverse(gc);
        self.arguments.traverse(gc);
    }
}

struct PartialApplicationDataDef<'a: 'b, 'b>(Callable<'a>, &'b [Value<'a>]);
impl<'a, 'b> Traverseable for PartialApplicationDataDef<'a, 'b> {
    fn traverse(&self, gc: &mut Gc) {
        self.0.traverse(gc);
        self.1.traverse(gc);
    }
}
unsafe impl<'a: 'b, 'b> DataDef for PartialApplicationDataDef<'a, 'b> {
    type Value = PartialApplicationData<'a>;
    fn size(&self) -> usize {
        use std::mem::size_of;
        size_of::<Callable<'a>>() + Array::<Cell<Value<'a>>>::size_of(self.1.len())
    }
    fn initialize<'w>(self,
                      mut result: WriteOnly<'w, PartialApplicationData<'a>>)
                      -> &'w mut PartialApplicationData<'a> {
        unsafe {
            let result = &mut *result.as_mut_ptr();
            result.function = self.0;
            result.arguments.initialize(self.1.iter().map(|v| Cell::new(v.clone())));
            result
        }
    }
}

impl<'a> PartialEq<Value<'a>> for Cell<Value<'a>> {
    fn eq(&self, other: &Value<'a>) -> bool {
        self.get() == *other
    }
}
impl<'a> PartialEq<Cell<Value<'a>>> for Value<'a> {
    fn eq(&self, other: &Cell<Value<'a>>) -> bool {
        *self == other.get()
    }
}

impl<'a> Traverseable for Value<'a> {
    fn traverse(&self, gc: &mut Gc) {
        match *self {
            String(ref data) => data.traverse(gc),
            Data(ref data) => data.traverse(gc),
            Function(ref data) => data.traverse(gc),
            Closure(ref data) => data.traverse(gc),
            Userdata(ref data) => data.data.traverse(gc),
            PartialApplication(ref data) => data.traverse(gc),
            Value::Lazy(ref lazy) => lazy.traverse(gc),
            Int(_) | Float(_) => (),
        }
    }
}

impl<'a> fmt::Debug for Value<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        struct Level<'a: 'b, 'b>(i32, &'b Value<'a>);
        struct LevelSlice<'a: 'b, 'b>(i32, &'b [Cell<Value<'a>>]);
        impl<'a, 'b> fmt::Debug for LevelSlice<'a, 'b> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                let level = self.0;
                if level <= 0 {
                    return Ok(());
                }
                for v in self.1 {
                    try!(write!(f, "{:?}", Level(level - 1, &v.get())));
                }
                Ok(())
            }
        }
        impl<'a, 'b> fmt::Debug for Level<'a, 'b> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                let level = self.0;
                if level <= 0 {
                    return Ok(());
                }
                match *self.1 {
                    Int(i) => write!(f, "{:?}", i),
                    Float(x) => write!(f, "{:?}f", x),
                    String(x) => write!(f, "{:?}", &*x),
                    Data(ref data) => {
                        write!(f,
                               "{{{:?} {:?}}}",
                               data.tag,
                               LevelSlice(level - 1, &data.fields))
                    }
                    Function(ref func) => write!(f, "<EXTERN {:?}>", &**func),
                    Closure(ref closure) => {
                        let p: *const _ = &*closure.function;
                        write!(f,
                               "<{:?} {:?} {:?}>",
                               closure.function.name,
                               p,
                               LevelSlice(level - 1, &closure.upvars))
                    }
                    PartialApplication(ref app) => {
                        let name = match app.function {
                            Callable::Closure(_) => "<CLOSURE>",
                            Callable::Extern(_) => "<EXTERN>",
                        };
                        write!(f,
                               "<App {:?} {:?}>",
                               name,
                               LevelSlice(level - 1, &app.arguments))
                    }
                    Userdata(ref data) => write!(f, "<Userdata {:?}>", data.ptr()),
                    Value::Lazy(_) => write!(f, "<lazy>"),
                }
            }
        }
        write!(f, "{:?}", Level(3, self))
    }
}

macro_rules! get_global {
    ($vm: ident, $i: expr) => (
        match $vm.globals[$i].value.get() {
            x => x
        }
    )
}

#[derive(Clone)]
pub struct RootedValue<'a: 'vm, 'vm> {
    vm: &'vm VM<'a>,
    value: Value<'a>,
}

impl<'a, 'vm> Drop for RootedValue<'a, 'vm> {
    fn drop(&mut self) {
        // TODO not safe if the root changes order of being dropped with another root
        self.vm.rooted_values.borrow_mut().pop();
    }
}

impl<'a, 'vm> fmt::Debug for RootedValue<'a, 'vm> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.value)
    }
}

impl<'a, 'vm> Deref for RootedValue<'a, 'vm> {
    type Target = Value<'a>;
    fn deref(&self) -> &Value<'a> {
        &self.value
    }
}

impl<'a, 'vm> RootedValue<'a, 'vm> {
    pub fn vm(&self) -> &'vm VM<'a> {
        self.vm
    }
}

pub struct Root<'a, T: ?Sized + 'a> {
    roots: &'a RefCell<Vec<GcPtr<Traverseable + 'static>>>,
    ptr: *const T,
}

impl<'a, T: ?Sized> Drop for Root<'a, T> {
    fn drop(&mut self) {
        // TODO not safe if the root changes order of being dropped with another root
        self.roots.borrow_mut().pop();
    }
}

impl<'a, T: ?Sized> Deref for Root<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.ptr }
    }
}

pub struct RootStr<'a>(Root<'a, Str>);

impl<'a> Deref for RootStr<'a> {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

#[derive(Eq, PartialEq)]
pub enum Status {
    Ok,
    Error,
}

pub struct ExternFunction<'a> {
    pub id: Symbol,
    pub args: VMIndex,
    pub function: Box<Fn(&VM<'a>) -> Status + 'static>,
}

impl<'a> PartialEq for ExternFunction<'a> {
    fn eq(&self, _: &ExternFunction<'a>) -> bool {
        false
    }
}

impl<'a> fmt::Debug for ExternFunction<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // read the v-table pointer of the Fn(..) type and print that
        let p: *const () = unsafe { ::std::mem::transmute_copy(&&*self.function) };
        write!(f, "{:?}", p)
    }
}

impl<'a> Traverseable for ExternFunction<'a> {
    fn traverse(&self, _: &mut Gc) {}
}

#[derive(Debug)]
pub struct Global<'a> {
    pub id: Symbol,
    pub typ: TcType,
    pub value: Cell<Value<'a>>,
}

impl<'a> Traverseable for Global<'a> {
    fn traverse(&self, gc: &mut Gc) {
        self.value.traverse(gc);
    }
}

impl<'a> Typed for Global<'a> {
    type Id = Symbol;
    fn env_type_of(&self, _: &TypeEnv) -> ASTType<Symbol> {
        self.typ.clone()
    }
}

#[derive(Debug)]
enum Named {
    GlobalFn(usize),
}

pub struct VM<'a> {
    globals: FixedVec<Global<'a>>,
    type_infos: RefCell<TypeInfos>,
    typeids: FixedMap<TypeId, TcType>,
    pub interner: RefCell<Interner>,
    symbols: RefCell<Symbols>,
    names: RefCell<HashMap<Symbol, Named>>,
    pub gc: RefCell<Gc>,
    roots: RefCell<Vec<GcPtr<Traverseable>>>,
    rooted_values: RefCell<Vec<Value<'a>>>,
    pub stack: RefCell<Stack<'a>>,
    macros: MacroEnv<VM<'a>>,
}

pub type VMResult<T> = Result<T, Error>;

#[derive(Debug)]
pub struct VMEnv<'a: 'b, 'b> {
    type_infos: Ref<'b, TypeInfos>,
    globals: &'b FixedVec<Global<'a>>,
    names: Ref<'b, HashMap<Symbol, Named>>,
    io_symbol: Symbol,
    io_arg: [types::Generic<Symbol>; 1],
}

impl<'a, 'b> CompilerEnv for VMEnv<'a, 'b> {
    fn find_var(&self, id: &Symbol) -> Option<Variable> {
        match self.names.get(id) {
            Some(&GlobalFn(index)) if index < self.globals.len() => {
                let g = &self.globals[index];
                Some(Variable::Global(index as VMIndex, &g.typ))
            }
            _ => self.type_infos.find_var(id),
        }
    }
}

impl<'a, 'b> KindEnv for VMEnv<'a, 'b> {
    fn find_kind(&self, type_name: Symbol) -> Option<RcKind> {
        self.type_infos
            .find_kind(type_name)
            .or_else(|| {
                if type_name == self.io_symbol {
                    Some(types::Kind::function(types::Kind::star(), types::Kind::star()))
                } else {
                    None
                }
            })
    }
}
impl<'a, 'b> TypeEnv for VMEnv<'a, 'b> {
    fn find_type(&self, id: &Symbol) -> Option<&TcType> {
        match self.names.get(id) {
            Some(&GlobalFn(index)) if index < self.globals.len() => {
                let g = &self.globals[index];
                Some(&g.typ)
            }
            _ => {
                self.type_infos
                    .id_to_type
                    .values()
                    .filter_map(|tuple| {
                        match *tuple.1 {
                            Type::Variants(ref ctors) => {
                                ctors.iter().find(|ctor| ctor.0 == *id).map(|t| &t.1)
                            }
                            _ => None,
                        }
                    })
                    .next()
                    .map(|ctor| ctor)
            }
        }
    }
    fn find_type_info(&self, id: &Symbol) -> Option<(&[types::Generic<Symbol>], Option<&TcType>)> {
        self.type_infos
            .find_type_info(id)
            .or_else(|| {
                if *id == self.io_symbol {
                    Some((&self.io_arg, None))
                } else {
                    None
                }
            })
    }
    fn find_record(&self, fields: &[Symbol]) -> Option<(&TcType, &TcType)> {
        self.type_infos.find_record(fields)
    }
}

pub struct Def<'a: 'b, 'b> {
    pub tag: VMTag,
    pub elems: &'b [Value<'a>],
}
unsafe impl<'a, 'b> DataDef for Def<'a, 'b> {
    type Value = DataStruct<'a>;
    fn size(&self) -> usize {
        use std::mem::size_of;
        size_of::<usize>() + Array::<Value<'a>>::size_of(self.elems.len())
    }
    fn initialize<'w>(self, mut result: WriteOnly<'w, DataStruct<'a>>) -> &'w mut DataStruct<'a> {
        unsafe {
            let result = &mut *result.as_mut_ptr();
            result.tag = self.tag;
            result.fields.initialize(self.elems.iter().map(|v| Cell::new(v.clone())));
            result
        }
    }
}

impl<'a, 'b> Traverseable for Def<'a, 'b> {
    fn traverse(&self, gc: &mut Gc) {
        self.elems.traverse(gc);
    }
}

struct Roots<'a: 'b, 'b> {
    globals: &'b FixedVec<Global<'a>>,
    stack: &'b Stack<'a>,
    interner: &'b mut Interner,
    roots: Ref<'b, Vec<GcPtr<Traverseable>>>,
    rooted_values: Ref<'b, Vec<Value<'a>>>,
}
impl<'a, 'b> Traverseable for Roots<'a, 'b> {
    fn traverse(&self, gc: &mut Gc) {
        for g in self.globals.borrow().iter() {
            g.traverse(gc);
        }
        self.stack.values.traverse(gc);
        // Also need to check the interned string table
        self.interner.traverse(gc);
        self.roots.traverse(gc);
        self.rooted_values.traverse(gc);
    }
}

impl<'a> VM<'a> {
    pub fn new() -> VM<'a> {
        let vm = VM {
            globals: FixedVec::new(),
            type_infos: RefCell::new(TypeInfos::new()),
            typeids: FixedMap::new(),
            symbols: RefCell::new(Symbols::new()),
            interner: RefCell::new(Interner::new()),
            names: RefCell::new(HashMap::new()),
            gc: RefCell::new(Gc::new()),
            stack: RefCell::new(Stack::new()),
            roots: RefCell::new(Vec::new()),
            rooted_values: RefCell::new(Vec::new()),
            macros: MacroEnv::new(),
        };
        // Enter the top level scope
        StackFrame::frame(vm.stack.borrow_mut(), 0, None);
        vm.add_types()
          .unwrap();
        vm.add_import();
        ::primitives::load(&vm).unwrap();
        vm
    }

    #[cfg(all(feature = "check", feature = "parser"))]
    fn add_import(&self) {
        self.macros.insert(self.symbol("import"), ::import::Import::new());
    }

    #[cfg(not(all(feature = "check", feature = "parser")))]
    fn add_import(&self) {
        let _ = &self.macros;
    }

    fn add_types(&self) -> Result<(), (TypeId, TcType)> {
        use api::generic::A;
        use api::Generic;
        let ref ids = self.typeids;
        try!(ids.try_insert(TypeId::of::<()>(), Type::unit()));
        try!(ids.try_insert(TypeId::of::<bool>(), Type::bool()));
        try!(ids.try_insert(TypeId::of::<VMInt>(), Type::int()));
        try!(ids.try_insert(TypeId::of::<f64>(), Type::float()));
        try!(ids.try_insert(TypeId::of::<::std::string::String>(), Type::string()));
        try!(ids.try_insert(TypeId::of::<char>(), Type::char()));
        let args = vec![types::Generic {
                            id: self.symbol("a"),
                            kind: types::Kind::star(),
                        }];
        let _ = self.register_type::<Lazy<Generic<A>>>("Lazy", args);
        Ok(())
    }

    pub fn add_bytecode(&self,
                        name: &str,
                        typ: TcType,
                        args: VMIndex,
                        instructions: Vec<Instruction>)
                        -> VMIndex {
        let id = self.symbol(name);
        let compiled_fn = CompiledFunction {
            args: args,
            id: id,
            typ: typ.clone(),
            instructions: instructions,
            inner_functions: vec![],
            strings: vec![],
        };
        let f = self.new_function(compiled_fn);
        let closure = self.alloc(&self.stack.borrow(), ClosureDataDef(f, &[]));
        self.names.borrow_mut().insert(id, GlobalFn(self.globals.len()));
        self.globals.push(Global {
            id: id,
            typ: typ,
            value: Cell::new(Closure(closure)),
        });
        self.globals.len() as VMIndex - 1
    }

    pub fn push(&self, v: Value<'a>) {
        self.stack.borrow_mut().push(v)
    }

    pub fn pop(&self) -> Value<'a> {
        self.stack
            .borrow_mut()
            .pop()
    }

    pub fn new_function(&self, f: CompiledFunction) -> GcPtr<BytecodeFunction> {
        BytecodeFunction::new(&mut self.gc.borrow_mut(), f)
    }

    pub fn get_type<T: ?Sized + Any>(&self) -> &TcType {
        let id = TypeId::of::<T>();
        self.typeids
            .get(&id)
            .unwrap_or_else(|| panic!("Expected type to be inserted before get_type call"))
    }

    pub fn global_exists(&self, name: &str) -> bool {
        let n = self.symbol(name);
        self.names.borrow().get(&n).is_some()
    }

    pub fn define_global<T>(&self, name: &str, value: T) -> Result<(), Error>
        where T: Pushable<'a>
    {
        let id = self.symbol(name);
        if self.names.borrow().contains_key(&id) {
            return Err(Error::Message(format!("{} is already defined", name)));
        }
        let (status, value) = {
            let mut stack = self.current_frame();
            let status = value.push(self, &mut stack);
            (status, stack.pop())
        };
        if status == Status::Error {
            return Err(Error::Message(format!("{:?}", value)));
        }
        let global = Global {
            id: id,
            typ: T::make_type(self),
            value: Cell::new(value),
        };
        self.names.borrow_mut().insert(id, GlobalFn(self.globals.len()));
        self.globals.push(global);
        Ok(())
    }

    pub fn get_global<'vm, T>(&'vm self, name: &str) -> Result<T, Error>
        where T: Getable<'a, 'vm> + VMType
    {
        let id = self.symbol(name);
        self.names
            .borrow()
            .get(&id)
            .map(|g| {
                match *g {
                    GlobalFn(i) => i,
                }
            })
            .ok_or_else(|| Error::Message(format!("Could not retrieve global `{}`", name)))
            .and_then(|i| {
                let global = &self.globals[i];
                if global.type_of() == T::make_type(self) {
                    T::from_value(self, global.value.get()).ok_or_else(|| {
                        Error::Message(format!("Could not retrieve global `{}`", name))
                    })
                } else {
                    Err(Error::Message(format!("Could not retrieve global `{}` as the types did \
                                                not match",
                                               name)))
                }
            })
    }

    pub fn register_type<T: ?Sized + Any>(&self,
                                          name: &str,
                                          args: Vec<types::Generic<Symbol>>)
                                          -> VMResult<&TcType> {
        let n = self.symbol(name);
        let mut type_infos = self.type_infos.borrow_mut();
        if type_infos.id_to_type.contains_key(&n) {
            Err(Error::Message(format!("Type '{}' has already been registered", name)))
        } else {
            let id = TypeId::of::<T>();
            let arg_types = args.iter().map(|g| Type::generic(g.clone())).collect();
            let typ = Type::data(types::TypeConstructor::Data(n), arg_types);
            self.typeids
                .try_insert(id, typ.clone())
                .expect("Id not inserted");
            let t = self.typeids.get(&id).unwrap();
            let ctor = Type::variants(vec![(n, typ.clone())]);
            type_infos.id_to_type.insert(n, (args, ctor.clone()));
            type_infos.type_to_id.insert(ctor, typ);
            Ok(t)
        }
    }

    pub fn get_symbols(&self) -> Ref<Symbols> {
        self.symbols.borrow()
    }

    pub fn get_mut_symbols(&self) -> RefMut<Symbols> {
        self.symbols.borrow_mut()
    }

    pub fn symbol_string(&self, s: Symbol) -> StdString {
        let symbols = self.symbols.borrow();
        StdString::from(symbols.string(&s))
    }

    pub fn symbol<N>(&self, name: N) -> Symbol
        where N: Into<NameBuf> + AsRef<Name>
    {
        let mut symbols = self.symbols.borrow_mut();
        symbols.symbol(name)
    }

    pub fn intern(&self, s: &str) -> InternedStr {
        self.interner.borrow_mut().intern(&mut *self.gc.borrow_mut(), s)
    }

    pub fn env<'b>(&'b self) -> VMEnv<'a, 'b> {
        VMEnv {
            type_infos: self.type_infos.borrow(),
            globals: &self.globals,
            names: self.names.borrow(),
            io_symbol: self.symbol("IO"),
            io_arg: [types::Generic {
                         id: self.symbol("a"),
                         kind: types::Kind::star(),
                     }],
        }
    }

    pub fn current_frame<'vm>(&'vm self) -> StackFrame<'a, 'vm> {
        let stack = self.stack.borrow_mut();
        StackFrame {
            frame: stack.frames.last().expect("Frame").clone(),
            stack: stack,
        }
    }

    pub fn collect(&self) {
        let stack = self.stack.borrow();
        self.with_roots(&stack, |gc, roots| {
            unsafe {
                gc.collect(roots);
            }
        })
    }

    pub fn root<T: Any>(&self, v: GcPtr<Box<Any>>) -> Option<Root<T>> {
        match v.downcast_ref::<T>().or_else(|| v.downcast_ref::<Box<T>>().map(|p| &**p)) {
            Some(ptr) => {
                self.roots.borrow_mut().push(v.as_traverseable());
                Some(Root {
                    roots: &self.roots,
                    ptr: ptr,
                })
            }
            None => None,
        }
    }

    pub fn root_string(&self, ptr: GcPtr<Str>) -> RootStr {
        self.roots.borrow_mut().push(ptr.as_traverseable());
        RootStr(Root {
            roots: &self.roots,
            ptr: &*ptr,
        })
    }

    pub fn root_value<'vm>(&'vm self, value: Value<'a>) -> RootedValue<'a, 'vm> {
        self.rooted_values.borrow_mut().push(value);
        RootedValue {
            vm: self,
            value: value,
        }
    }

    pub fn new_data(&self, tag: VMTag, fields: &[Value<'a>]) -> Value<'a> {
        Data(self.gc.borrow_mut().alloc(Def {
            tag: tag,
            elems: fields,
        }))
    }

    fn with_roots<F, R>(&self, stack: &Stack<'a>, f: F) -> R
        where F: for<'b> FnOnce(&mut Gc, Roots<'a, 'b>) -> R
    {
        // For this to be safe we require that the received stack is the same one that is in this
        // VM
        assert!(unsafe {
            stack as *const _ as usize >= &self.stack as *const _ as usize &&
            stack as *const _ as usize <= (&self.stack as *const _).offset(1) as usize
        });
        let mut interner = self.interner.borrow_mut();
        let roots = Roots {
            globals: &self.globals,
            stack: stack,
            interner: &mut *interner,
            roots: self.roots.borrow(),
            rooted_values: self.rooted_values.borrow(),
        };
        let mut gc = self.gc.borrow_mut();
        f(&mut gc, roots)
    }

    pub fn alloc<D>(&self, stack: &Stack<'a>, def: D) -> GcPtr<D::Value>
        where D: DataDef + Traverseable
    {
        self.with_roots(stack,
                        |gc, roots| unsafe { gc.alloc_and_collect(roots, def) })
    }

    ///Calls a module, allowed to to run IO expressions
    pub fn call_module(&self,
                       typ: &TcType,
                       closure: GcPtr<ClosureData<'a>>)
                       -> VMResult<Value<'a>> {
        let value = try!(self.call_bytecode(closure));
        if let Type::Data(types::TypeConstructor::Data(id), _) = **typ {
            if id == self.symbol("IO") {
                debug!("Run IO {:?}", value);
                self.push(Int(0));// Dummy value to fill the place of the function for TailCall
                self.push(value);
                self.push(Int(0));
                let mut stack = StackFrame::frame(self.stack.borrow_mut(), 2, None);
                stack = try!(self.call_function(stack, 1))
                            .expect("call_module to have the stack remaining");
                let result = stack.pop();
                while stack.len() > 0 {
                    stack.pop();
                }
                stack.exit_scope();
                return Ok(result);
            }
        }
        Ok(value)
    }

    fn call_bytecode(&self, closure: GcPtr<ClosureData<'a>>) -> VMResult<Value<'a>> {
        self.push(Closure(closure));
        let stack = StackFrame::frame(self.stack.borrow_mut(), 0, Some(Callable::Closure(closure)));
        try!(self.execute(stack));
        let mut stack = self.stack.borrow_mut();
        Ok(stack.pop())
    }

    fn execute_callable<'b>(&'b self,
                            mut stack: StackFrame<'a, 'b>,
                            function: &Callable<'a>,
                            excess: bool)
                            -> Result<StackFrame<'a, 'b>, Error> {
        match *function {
            Callable::Closure(closure) => {
                stack = stack.enter_scope(closure.function.args, Some(Callable::Closure(closure)));
                stack.frame.excess = excess;
                stack.stack.frames.last_mut().unwrap().excess = excess;
                Ok(stack)
            }
            Callable::Extern(ref ext) => {
                assert!(stack.len() >= ext.args + 1);
                let function_index = stack.len() - ext.args - 1;
                debug!("------- {} {:?}", function_index, &stack[..]);
                Ok(stack.enter_scope(ext.args, Some(Callable::Extern(*ext))))
            }
        }
    }

    fn execute_function<'b>(&'b self,
                            stack: StackFrame<'a, 'b>,
                            function: &ExternFunction<'a>)
                            -> Result<StackFrame<'a, 'b>, Error> {
        debug!("CALL EXTERN {}", self.symbols.borrow().string(&function.id));
        // Make sure that the stack is not borrowed during the external function call
        // Necessary since we do not know what will happen during the function call
        let StackFrame { stack, frame } = stack;
        drop(stack);
        let status = (function.function)(self);
        let mut stack = StackFrame {
            stack: self.stack.borrow_mut(),
            frame: frame,
        };
        let result = stack.pop();
        while stack.len() > 0 {
            debug!("{} {:?}", stack.len(), &stack[..]);
            stack.pop();
        }
        stack = try!(stack.exit_scope()
                          .ok_or_else(|| {
                              Error::Message(StdString::from("Poped the last frame in \
                                                              execute_function"))
                          }));
        stack.pop();// Pop function
        stack.push(result);
        match status {
            Status::Ok => Ok(stack),
            Status::Error => {
                match stack.pop() {
                    String(s) => Err(Error::Message(s.to_string())),
                    _ => Err(Error::Message("Unexpected panic in VM".to_string())),
                }
            }
        }
    }

    fn call_function_with_upvars<'b>(&'b self,
                                     mut stack: StackFrame<'a, 'b>,
                                     args: VMIndex,
                                     required_args: VMIndex,
                                     callable: Callable<'a>)
                                     -> Result<StackFrame<'a, 'b>, Error> {
        debug!("cmp {} {} {:?} {:?}", args, required_args, callable, {
            let function_index = stack.len() - 1 - args;
            &(*stack)[(function_index + 1) as usize..]
        });
        match args.cmp(&required_args) {
            Ordering::Equal => self.execute_callable(stack, &callable, false),
            Ordering::Less => {
                let app = {
                    let fields = &stack[stack.len() - args..];
                    let def = PartialApplicationDataDef(callable, fields);
                    PartialApplication(self.alloc(&stack.stack, def))
                };
                for _ in 0..(args + 1) {
                    stack.pop();
                }
                stack.push(app);
                Ok(stack)
            }
            Ordering::Greater => {
                let excess_args = args - required_args;
                let d = {
                    let fields = &stack[stack.len() - excess_args..];
                    self.alloc(&stack.stack,
                               Def {
                                   tag: 0,
                                   elems: fields,
                               })
                };
                for _ in 0..excess_args {
                    stack.pop();
                }
                // Insert the excess args before the actual closure so it does not get
                // collected
                let offset = stack.len() - required_args - 1;
                stack.insert_slice(offset, &[Cell::new(Data(d))]);
                debug!("xxxxxx {:?}\n{:?}", &(*stack)[..], stack.stack.frames);
                self.execute_callable(stack, &callable, true)
            }
        }
    }

    fn do_call<'b>(&'b self,
                   mut stack: StackFrame<'a, 'b>,
                   args: VMIndex)
                   -> Result<StackFrame<'a, 'b>, Error> {
        let function_index = stack.len() - 1 - args;
        debug!("Do call {:?} {:?}",
               stack[function_index],
               &(*stack)[(function_index + 1) as usize..]);
        match stack[function_index].clone() {
            Function(ref f) => {
                let callable = Callable::Extern(f.clone());
                self.call_function_with_upvars(stack, args, f.args, callable)
            }
            Closure(ref closure) => {
                let callable = Callable::Closure(closure.clone());
                self.call_function_with_upvars(stack, args, closure.function.args, callable)
            }
            PartialApplication(app) => {
                let total_args = app.arguments.len() as VMIndex + args;
                let offset = stack.len() - args;
                stack.insert_slice(offset, &app.arguments);
                self.call_function_with_upvars(stack, total_args, app.function.args(), app.function)
            }
            x => return Err(Error::Message(format!("Cannot call {:?}", x))),
        }
    }

    pub fn call_function<'b>(&'b self,
                         mut stack: StackFrame<'a, 'b>,
                         args: VMIndex)
                         -> Result<Option<StackFrame<'a, 'b>>, Error> {
        stack = try!(self.do_call(stack, args));
        self.execute(stack)
    }

    fn execute<'b>(&'b self,
                   stack: StackFrame<'a, 'b>)
                   -> Result<Option<StackFrame<'a, 'b>>, Error> {
        let mut maybe_stack = Some(stack);
        while let Some(mut stack) = maybe_stack {
            debug!("STACK\n{:?}", stack.stack.frames);
            maybe_stack = match stack.frame.function {
                None => return Ok(Some(stack)),
                Some(Callable::Extern(ext)) => {
                    if stack.frame.instruction_index != 0 {
                        // This function was already called
                        return Ok(Some(stack));
                    } else {
                        stack.frame.instruction_index = 1;
                        stack.stack.frames.last_mut().unwrap().instruction_index = 1;
                        Some(try!(self.execute_function(stack, &ext)))
                    }
                }
                Some(Callable::Closure(closure)) => {
                    // Tail calls into extern functions at the top level will drop the last
                    // stackframe so just return immedietly
                    if stack.stack.frames.len() == 0 {
                        return Ok(Some(stack));
                    }
                    let instruction_index = stack.frame.instruction_index;
                    {
                        let symbols = self.symbols.borrow();
                        debug!("Continue with {}\nAt: {}/{}",
                               closure.function
                                      .name
                                      .as_ref()
                                      .map(|s| symbols.string(s))
                                      .unwrap_or("<UNKNOWN>"),
                               instruction_index,
                               closure.function.instructions.len());
                    }
                    let new_stack = try!(self.execute_(stack,
                                                       instruction_index,
                                                       &closure.function.instructions,
                                                       &closure.function));
                    new_stack
                }
            };
        }
        Ok(maybe_stack)
    }

    fn execute_<'b>(&'b self,
                    mut stack: StackFrame<'a, 'b>,
                    mut index: usize,
                    instructions: &[Instruction],
                    function: &BytecodeFunction)
                    -> Result<Option<StackFrame<'a, 'b>>, Error> {
        {
            let symbols = self.symbols.borrow();
            debug!(">>>\nEnter frame {}: {:?}\n{:?}",
                   function.name
                           .as_ref()
                           .map(|s| symbols.string(s))
                           .unwrap_or("<UNKNOWN>"),
                   &stack[..],
                   stack.frame);
        }
        while let Some(&instr) = instructions.get(index) {
            debug_instruction(&stack, index, instr);
            match instr {
                Push(i) => {
                    let v = stack[i].clone();
                    stack.push(v);
                }
                PushInt(i) => {
                    stack.push(Int(i));
                }
                PushString(string_index) => {
                    stack.push(String(function.strings[string_index as usize].inner()));
                }
                PushGlobal(i) => {
                    let x = get_global!(self, i as usize);
                    stack.push(x);
                }
                PushFloat(f) => stack.push(Float(f)),
                Call(args) => {
                    stack.frame.instruction_index = index + 1;
                    return self.do_call(stack, args).map(Some);
                }
                TailCall(mut args) => {
                    let mut amount = stack.len() - args;
                    if stack.frame.excess {
                        amount += 1;
                        let i = stack.stack.values.len() - stack.len() as usize - 2;
                        match stack.stack.values[i] {
                            Data(excess) => {
                                debug!("TailCall: Push excess args {:?}", &excess.fields);
                                for value in &excess.fields {
                                    stack.push(value.get());
                                }
                                args += excess.fields.len() as VMIndex;
                            }
                            _ => panic!("Expected excess args"),
                        }
                    }
                    stack = match stack.exit_scope() {
                        Some(stack) => stack,
                        None => return Ok(None),
                    };
                    debug!("{} {} {:?}", stack.len(), amount, &stack[..]);
                    let end = stack.len() - args - 1;
                    stack.remove_range(end - amount, end);
                    debug!("{:?}", &stack[..]);
                    return self.do_call(stack, args).map(Some);
                }
                Construct(tag, args) => {
                    let d = {
                        let fields = &stack[stack.len() - args..];
                        self.alloc(&stack.stack,
                                   Def {
                                       tag: tag,
                                       elems: fields,
                                   })
                    };
                    for _ in 0..args {
                        stack.pop();
                    }
                    stack.push(Data(d));
                }
                GetField(i) => {
                    match stack.pop() {
                        Data(data) => {
                            let v = data.fields[i as usize].get();
                            stack.push(v);
                        }
                        x => return Err(Error::Message(format!("GetField on {:?}", x))),
                    }
                }
                TestTag(tag) => {
                    let x = match stack.top() {
                        Data(ref data) => {
                            if data.tag == tag {
                                1
                            } else {
                                0
                            }
                        }
                        _ => {
                            return Err(Error::Message("Op TestTag called on non data type"
                                                          .to_string()))
                        }
                    };
                    stack.push(Int(x));
                }
                Split => {
                    match stack.pop() {
                        Data(data) => {
                            for field in data.fields.iter().map(|x| x.get()) {
                                stack.push(field.clone());
                            }
                        }
                        _ => {
                            return Err(Error::Message("Op Split called on non data type"
                                                          .to_string()))
                        }
                    }
                }
                Jump(i) => {
                    index = i as usize;
                    continue;
                }
                CJump(i) => {
                    match stack.pop() {
                        Int(0) => (),
                        _ => {
                            index = i as usize;
                            continue;
                        }
                    }
                }
                Pop(n) => {
                    for _ in 0..n {
                        stack.pop();
                    }
                }
                Slide(n) => {
                    debug!("{:?}", &stack[..]);
                    let v = stack.pop();
                    for _ in 0..n {
                        stack.pop();
                    }
                    stack.push(v);
                }
                GetIndex => {
                    let index = stack.pop();
                    let array = stack.pop();
                    match (array, index) {
                        (Data(array), Int(index)) => {
                            let v = array.fields[index as usize].get();
                            stack.push(v);
                        }
                        (x, y) => {
                            return Err(Error::Message(format!("Op GetIndex called on invalid \
                                                               types {:?} {:?}",
                                                              x,
                                                              y)))
                        }
                    }
                }
                SetIndex => {
                    let value = stack.pop();
                    let index = stack.pop();
                    let array = stack.pop();
                    match (array, index) {
                        (Data(array), Int(index)) => {
                            array.fields[index as usize].set(value);
                        }
                        (x, y) => {
                            return Err(Error::Message(format!("Op SetIndex called on invalid \
                                                               types {:?} {:?}",
                                                              x,
                                                              y)))
                        }
                    }
                }
                MakeClosure(fi, n) => {
                    let closure = {
                        let args = &stack[stack.len() - n..];
                        let func = function.inner_functions[fi as usize];
                        Closure(self.alloc(&stack.stack, ClosureDataDef(func, args)))
                    };
                    for _ in 0..n {
                        stack.pop();
                    }
                    stack.push(closure);
                }
                NewClosure(fi, n) => {
                    let closure = {
                        // Use dummy variables until it is filled
                        let args = [Int(0); 128];
                        let func = function.inner_functions[fi as usize];
                        Closure(self.alloc(&stack.stack, ClosureDataDef(func, &args[..n as usize])))
                    };
                    stack.push(closure);
                }
                CloseClosure(n) => {
                    let i = stack.len() - n - 1;
                    match stack[i] {
                        Closure(closure) => {
                            for var in closure.upvars.iter().rev() {
                                var.set(stack.pop());
                            }
                            stack.pop();//Remove the closure
                        }
                        x => panic!("Expected closure, got {:?}", x),
                    }
                }
                PushUpVar(i) => {
                    let v = stack.get_upvar(i).clone();
                    stack.push(v);
                }
                AddInt => binop_int(&mut stack, |l, r| l + r),
                SubtractInt => binop_int(&mut stack, |l, r| l - r),
                MultiplyInt => binop_int(&mut stack, |l, r| l * r),
                IntLT => {
                    binop_int(&mut stack, |l, r| {
                        if l < r {
                            1
                        } else {
                            0
                        }
                    })
                }
                IntEQ => {
                    binop_int(&mut stack, |l, r| {
                        if l == r {
                            1
                        } else {
                            0
                        }
                    })
                }

                AddFloat => binop_float(&mut stack, |l, r| l + r),
                SubtractFloat => binop_float(&mut stack, |l, r| l - r),
                MultiplyFloat => binop_float(&mut stack, |l, r| l * r),
                FloatLT => {
                    binop(&mut stack, |l, r| {
                        match (l, r) {
                            (Float(l), Float(r)) => {
                                Int(if l < r {
                                    1
                                } else {
                                    0
                                })
                            }
                            _ => panic!(),
                        }
                    })
                }
                FloatEQ => {
                    binop(&mut stack, |l, r| {
                        match (l, r) {
                            (Float(l), Float(r)) => {
                                Int(if l == r {
                                    1
                                } else {
                                    0
                                })
                            }
                            _ => panic!(),
                        }
                    })
                }
            }
            index += 1;
        }
        if stack.len() != 0 {
            debug!("--> {:?}", stack.top());
        } else {
            debug!("--> ()");
        }
        let result = stack.pop();
        debug!("Return {:?}", result);
        let len = stack.len();
        for _ in 0..(len + 1) {
            stack.pop();
        }
        if stack.frame.excess {
            match stack.pop() {
                Data(excess) => {
                    debug!("Push excess args {:?}", &excess.fields);
                    stack.push(result);
                    for value in &excess.fields {
                        stack.push(value.get());
                    }
                    stack = match stack.exit_scope() {
                        Some(stack) => stack,
                        None => return Ok(None),
                    };
                    self.do_call(stack, excess.fields.len() as VMIndex).map(Some)
                }
                x => panic!("Expected excess arguments found {:?}", x),
            }
        } else {
            stack.push(result);
            Ok(stack.exit_scope())
        }
    }
}

#[inline]
fn binop<'a, 'b, F>(stack: &mut StackFrame<'a, 'b>, f: F)
    where F: FnOnce(Value<'a>, Value<'a>) -> Value<'a>
{
    let r = stack.pop();
    let l = stack.pop();
    stack.push(f(l, r));
}
#[inline]
fn binop_int<F>(stack: &mut StackFrame, f: F)
    where F: FnOnce(VMInt, VMInt) -> VMInt
{
    binop(stack, move |l, r| {
        match (l, r) {
            (Int(l), Int(r)) => Int(f(l, r)),
            (l, r) => panic!("{:?} `intOp` {:?}", l, r),
        }
    })
}
#[inline]
fn binop_float<F>(stack: &mut StackFrame, f: F)
    where F: FnOnce(f64, f64) -> f64
{
    binop(stack, move |l, r| {
        match (l, r) {
            (Float(l), Float(r)) => Float(f(l, r)),
            (l, r) => panic!("{:?} `floatOp` {:?}", l, r),
        }
    })
}

fn debug_instruction(stack: &StackFrame, index: usize, instr: Instruction) {
    debug!("{:?}: {:?} {:?}",
           index,
           instr,
           match instr {
               Push(i) => stack.get(i as usize).cloned(),
               NewClosure(..) => Some(Int(stack.len() as isize)),
               MakeClosure(..) => Some(Int(stack.len() as isize)),
               _ => None,
           });
}

#[cfg(not(all(feature = "check", feature = "parser")))]
quick_error! {
    #[derive(Debug)]
    pub enum Error {
        IO(err: ::std::io::Error) {
            description(err.description())
            display("{}", err)
            from()
        }
        Message(err: StdString) {
            display("{}", err)
        }
        Macro(err: ::base::macros::Error) {
            description(err.description())
            display("{}", err)
            from()
        }
    }
}

#[cfg(all(feature = "check", feature = "parser"))]
quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Parse(err: ::parser::Error) {
            description(err.description())
            display("{}", err)
            from()
        }
        Typecheck(err: ::base::error::InFile<::check::typecheck::TypeError<StdString>>) {
            description(err.description())
            display("{}", err)
            from()
        }
        IO(err: ::std::io::Error) {
            description(err.description())
            display("{}", err)
            from()
        }
        Message(err: StdString) {
            display("{}", err)
        }
        Macro(err: ::base::error::Errors<::base::macros::Error>) {
            description(err.description())
            display("{}", err)
            from()
        }
    }
}

#[cfg(all(feature = "check", feature = "parser"))]
fn include_implicit_prelude(vm: &VM, name: &str, expr: &mut ast::LExpr<ast::TcIdent<Symbol>>) {
    use std::mem;
    if name == "std.prelude" {
        return;
    }

    let prelude_import = r#"
let __implicit_prelude = import "std/prelude.hs"
and { Num, Eq, Ord, Show, Functor, Monad, Option, Result, not } = __implicit_prelude
in
let { (+), (-), (*) } = __implicit_prelude.num_Int
and { (==) } = __implicit_prelude.eq_Int
and { (<), (<=), (=>), (>) } = __implicit_prelude.make_Ord __implicit_prelude.ord_Int
in
let { (+), (-), (*) } = __implicit_prelude.num_Float
and { (==) } = __implicit_prelude.eq_Float
and { (<), (<=), (=>), (>) } = __implicit_prelude.make_Ord __implicit_prelude.ord_Float
in 0
"#;
    let prelude_expr = parse_expr(vm, "", prelude_import).unwrap();
    let original_expr = mem::replace(expr, prelude_expr);
    fn assign_last_body(l: &mut ast::LExpr<ast::TcIdent<Symbol>>,
                        original_expr: ast::LExpr<ast::TcIdent<Symbol>>) {
        match l.value {
            ast::Expr::Let(_, ref mut e) => {
                assign_last_body(e, original_expr);
            }
            _ => *l = original_expr,
        }
    }
    assign_last_body(expr, original_expr);
}

#[cfg(all(feature = "check", feature = "parser"))]
fn compile_script(vm: &VM,
                  filename: &str,
                  expr: &ast::LExpr<ast::TcIdent<Symbol>>)
                  -> CompiledFunction {
    use base::symbol::SymbolModule;
    use compiler::Compiler;
    debug!("Compile `{}`", filename);
    let mut function = {
        let env = vm.env();
        let mut interner = vm.interner.borrow_mut();
        let mut gc = vm.gc.borrow_mut();
        let mut symbols = vm.symbols.borrow_mut();
        let name = Name::new(filename);
        let name = NameBuf::from(name.module());
        let symbols = SymbolModule::new(StdString::from(name.as_ref()), &mut symbols);
        let mut compiler = Compiler::new(&env, &mut interner, &mut gc, symbols);
        compiler.compile_expr(&expr)
    };
    function.id = vm.symbols.borrow_mut().symbol(filename);
    function
}

/// Compiles `input` and if it is successful runs the resulting code and stores the resulting value
/// in the global variable named by running `filename_to_module` on `filename`.
///
/// If at any point the function fails the resulting error is returned and nothing is added to the
/// VM.
#[cfg(all(feature = "check", feature = "parser"))]
pub fn load_script(vm: &VM, filename: &str, input: &str) -> Result<(), Error> {
    load_script2(vm, filename, input, true)
}

#[cfg(all(feature = "check", feature = "parser"))]
fn load_script2(vm: &VM, filename: &str, input: &str, implicit_prelude: bool) -> Result<(), Error> {
    let (expr, typ) = try!(typecheck_expr(vm, filename, input, implicit_prelude));
    let function = compile_script(vm, filename, &expr);
    let function = BytecodeFunction::new(&mut vm.gc.borrow_mut(), function);
    let closure = vm.gc.borrow_mut().alloc(ClosureDataDef(function, &[]));
    let value = try!(vm.call_module(&typ, closure));
    vm.names.borrow_mut().insert(function.name.unwrap(), GlobalFn(vm.globals.len()));
    vm.globals.push(Global {
        id: function.name.unwrap(),
        typ: typ,
        value: Cell::new(value),
    });
    Ok(())
}

pub fn filename_to_module(filename: &str) -> StdString {
    use std::path::Path;
    let path = Path::new(filename);
    let name = path.extension()
                   .map(|ext| {
                       ext.to_str()
                          .map(|ext| &filename[..filename.len() - ext.len() - 1])
                          .unwrap_or(filename)
                   })
                   .expect("filename");

    name.replace("/", ".")
}

/// Loads `filename` and compiles and runs its input by calling `load_script`
#[cfg(all(feature = "check", feature = "parser"))]
pub fn load_file(vm: &VM, filename: &str) -> Result<(), Error> {
    use std::fs::File;
    use std::io::Read;
    let mut file = try!(File::open(filename));
    let mut buffer = ::std::string::String::new();
    try!(file.read_to_string(&mut buffer));
    drop(file);
    let name = filename_to_module(filename);
    load_script(vm, &name, &buffer)
}

#[cfg(feature = "parser")]
pub fn parse_expr(vm: &VM,
                  file: &str,
                  input: &str)
                  -> Result<ast::LExpr<ast::TcIdent<Symbol>>, ::parser::Error> {
    use base::symbol::SymbolModule;
    let mut symbols = vm.symbols.borrow_mut();
    Ok(try!(::parser::parse_tc(&mut SymbolModule::new(file.into(), &mut symbols), input)))
}

#[cfg(all(feature = "check", feature = "parser"))]
pub fn typecheck_expr<'a>(vm: &VM<'a>,
                          file: &str,
                          expr_str: &str,
                          implicit_prelude: bool)
                          -> Result<(ast::LExpr<ast::TcIdent<Symbol>>, TcType), Error> {
    use check::typecheck::Typecheck;
    use base::error;
    let mut expr = try!(parse_expr(vm, file, expr_str));
    if implicit_prelude {
        include_implicit_prelude(vm, file, &mut expr);
    }
    try!(vm.macros.run(vm, &mut expr));
    let env = vm.env();
    let mut symbols = vm.symbols.borrow_mut();
    let mut tc = Typecheck::new(file.into(), &mut symbols, &env);
    let typ = try!(tc.typecheck_expr(&mut expr)
                     .map_err(|err| error::InFile::new(StdString::from(file), expr_str, err)));
    Ok((expr, typ))
}

/// Compiles and runs the expression in `expr_str`. If successful the value from running the
/// expression is returned
#[cfg(all(feature = "check", feature = "parser"))]
pub fn run_expr<'a, 'vm>(vm: &'vm VM<'a>,
                         name: &str,
                         expr_str: &str)
                         -> Result<RootedValue<'a, 'vm>, Error> {
    run_expr2(vm, name, expr_str, true)
}

#[cfg(all(feature = "check", feature = "parser"))]
fn run_expr2<'a, 'vm>(vm: &'vm VM<'a>,
                      name: &str,
                      expr_str: &str,
                      implicit_prelude: bool)
                      -> Result<RootedValue<'a, 'vm>, Error> {
    let (expr, typ) = try!(typecheck_expr(vm, name, expr_str, implicit_prelude));
    let mut function = compile_script(vm, name, &expr);
    function.id = vm.symbols.borrow_mut().symbol(name);
    let function = vm.new_function(function);
    let closure = vm.gc.borrow_mut().alloc(ClosureDataDef(function, &[]));
    let value = try!(vm.call_module(&typ, closure));
    Ok(vm.root_value(value))
}

#[cfg(all(test, feature = "check", feature = "parser"))]
pub mod tests {
    use vm::{VM, Value};
    use vm::Value::{Float, Int};
    use stack::StackFrame;

    pub fn load_script(vm: &VM, filename: &str, input: &str) -> Result<(), super::Error> {
        super::load_script2(vm, filename, input, false)
    }

    pub fn run_expr<'a>(vm: &VM<'a>, s: &str) -> Value<'a> {
        super::run_expr2(vm, "<top>", s, false).unwrap_or_else(|err| panic!("{}", err)).value
    }

    macro_rules! test_expr {
        ($name: ident, $expr: expr, $value: expr) => {
            #[test]
            fn $name() {
                let _ = ::env_logger::init();
                let mut vm = VM::new();
                let value = run_expr(&mut vm, $expr);
                assert_eq!(value, $value);
            }
        };
        ($name: ident, $expr: expr) => {
            #[test]
            fn $name() {
                let _ = ::env_logger::init();
                let mut vm = VM::new();
                run_expr(&mut vm, $expr);
            }
        }
    }

    test_expr!{ pass_function_value,
r"
let lazy: () -> Int = \x -> 42 in
let test: (() -> Int) -> Int = \f -> f () #Int+ 10
in test lazy
",
Int(52)
}

    test_expr!{ lambda,
r"
let y = 100 in
let f = \x -> y #Int+ x #Int+ 1
in f(22)
",
Int(123)
}

    test_expr!{ add_operator,
r"
let (+) = \x y -> x #Int+ y in 1 + 2 + 3
",
Int(6)
}
    #[test]
    fn record() {
        let _ = ::env_logger::init();
        let text = r"
{ x = 0, y = 1.0, z = {} }
";
        let mut vm = VM::new();
        let value = run_expr(&mut vm, text);
        let unit = vm.new_data(0, &mut []);
        assert_eq!(value, vm.new_data(0, &mut [Int(0), Float(1.0), unit]));
    }

    #[test]
    fn add_record() {
        let _ = ::env_logger::init();
        let text = r"
type T = { x: Int, y: Int } in
let add = \l r -> { x = l.x #Int+ r.x, y = l.y #Int+ r.y } in
add { x = 0, y = 1 } { x = 1, y = 1 }
";
        let mut vm = VM::new();
        let value = run_expr(&mut vm, text);
        assert_eq!(value, vm.new_data(0, &mut [Int(1), Int(2)]));
    }
    #[test]
    fn script() {
        let _ = ::env_logger::init();
        let text = r"
type T = { x: Int, y: Int } in
let add = \l r -> { x = l.x #Int+ r.x, y = l.y #Int+ r.y } in
let sub = \l r -> { x = l.x #Int- r.x, y = l.y #Int- r.y } in
{ T, add, sub }
";
        let mut vm = VM::new();
        load_script(&mut vm, "Vec", text).unwrap_or_else(|err| panic!("{}", err));

        let script = r#"
let { T, add, sub } = Vec
in add { x = 10, y = 5 } { x = 1, y = 2 }
"#;
        let value = run_expr(&mut vm, script);
        assert_eq!(value, vm.new_data(0, &mut [Int(11), Int(7)]));
    }
    #[test]
    fn adt() {
        let _ = ::env_logger::init();
        let text = r"
type Option a = | None | Some a
in Some 1
";
        let mut vm = VM::new();
        let value = run_expr(&mut vm, text);
        assert_eq!(value, vm.new_data(1, &mut [Int(1)]));
    }


    test_expr!{ recursive_function,
r"
let fib x = if x #Int< 3
            then 1
            else fib (x #Int- 1) #Int+ fib (x #Int- 2)
in fib 7
",
Int(13)
}

    test_expr!{ mutually_recursive_function,
r"
let f x = if x #Int< 0
          then x
          else g x
and g x = f (x #Int- 1)
in g 3
",
Int(-1)
}

    test_expr!{ no_capture_self_function,
r"
let x = 2 in
let f y = x
in f 4
",
Int(2)
}

    #[test]
    fn insert_stack_slice() {
        use std::cell::Cell;

        let _ = ::env_logger::init();
        let vm = VM::new();
        let mut stack = StackFrame::frame(vm.stack.borrow_mut(), 0, None);
        stack.push(Int(0));
        stack.insert_slice(0, &[Cell::new(Int(2)), Cell::new(Int(1))]);
        assert_eq!(&stack[..], [Int(2), Int(1), Int(0)]);
        stack = stack.enter_scope(2, None);
        stack.insert_slice(1, &[Cell::new(Int(10))]);
        assert_eq!(&stack[..], [Int(1), Int(10), Int(0)]);
        stack.insert_slice(1, &[]);
        assert_eq!(&stack[..], [Int(1), Int(10), Int(0)]);
        stack.insert_slice(2,
                           &[Cell::new(Int(4)), Cell::new(Int(5)), Cell::new(Int(6))]);
        assert_eq!(&stack[..],
                   [Int(1), Int(10), Int(4), Int(5), Int(6), Int(0)]);
        stack.exit_scope();
    }

    test_expr!{ partial_application,
r"
let f x y = x #Int+ y in
let g = f 10
in g 2 #Int+ g 3
",
Int(25)
}

    test_expr!{ partial_application2,
r"
let f x y z = x #Int+ y #Int+ z in
let g = f 10 in
let h = g 20
in h 2 #Int+ g 10 3
",
Int(55)
}

    test_expr!{ to_many_args_application,
r"
let f x = \y -> x #Int+ y in
let g = f 20
in f 10 2 #Int+ g 3
",
Int(35)
}

    test_expr!{ to_many_args_partial_application_twice,
r"
let f x = \y z -> x #Int+ y #Int+ z in
let g = f 20 5
in f 10 2 1 #Int+ g 2
",
Int(40)
}

    test_expr!{ print_int,
r"
io.print_int 123
"
}

    test_expr!{ no_io_eval,
r#"
let x = io_bind (io.print_int 1) (\x -> error "NOOOOOOOO")
in { x }
"#
}

    test_expr!{ char,
r#"
'a'
"#,
Int('a' as isize)
}

    #[test]
    fn non_exhaustive_pattern() {
        let _ = ::env_logger::init();
        let text = r"
type AB = | A | B in
case A of
    | B -> True
";
        let mut vm = VM::new();
        let result = super::run_expr(&mut vm, "<top>", text);
        assert!(result.is_err());
    }

    test_expr!{ record_pattern,
r#"
case { x = 1, y = "abc" } of
    | { x, y = z } -> x #Int+ string_prim.length z
"#,
Int(4)
}

    test_expr!{ let_record_pattern,
r#"
let (+) x y = x #Int+ y
in
let a = { x = 10, y = "abc" }
in
let {x, y = z} = a
in x + string_prim.length z
"#,
Int(13)
}

    test_expr!{ partial_record_pattern,
r#"
type A = { x: Int, y: Float } in
let x = { x = 1, y = 2.0 }
in case x of
    | { y } -> y
"#,
Float(2.0)
}

    test_expr!{ record_let_adjust,
r#"
let x = \z -> let { x, y } = { x = 1, y = 2 } in z in
let a = 3
in a
"#,
Int(3)
}

    test_expr!{ unit_expr,
r#"
let x = ()
and y = 1
in y
"#,
Int(1)
}

    test_expr!{ let_not_in_tail_position,
r#"
1 #Int+ let x = 2 in x
"#,
Int(3)
}

    test_expr!{ field_access_not_in_tail_position,
r#"
let id x = x
in (id { x = 1 }).x
"#,
Int(1)
}

    test_expr!{ module_function,
r#"let x = string_prim.length "test" in x"#,
Int(4)
}

    test_expr!{ io_print,
r#"io.print "123" "#
}

    test_expr!{ array,
r#"
let arr = [1,2,3]
in array.index arr 0 #Int== 1
&& array.length arr #Int== 3
&& array.length (array.append arr arr) #Int== array.length arr #Int* 2"#,
Int(1)
}
    #[test]
    fn overloaded_bindings() {
        let _ = ::env_logger::init();
        let text = r#"
let (+) x y = x #Int+ y
in
let (+) x y = x #Float+ y
in
{ x = 1 + 2, y = 1.0 + 2.0 }
"#;
        let mut vm = VM::new();
        let result = run_expr(&mut vm, text);
        assert_eq!(result, vm.new_data(0, &mut [Int(3), Float(3.0)]));
    }

    #[test]
    fn through_overloaded_alias() {
        let _ = ::env_logger::init();
        let text = r#"
type Test a = { id : a -> a }
in
let test_Int: Test Int = { id = \x -> 0 }
in
let test_String: Test String = { id = \x -> "" }
in
let { id } = test_Int
in
let { id } = test_String
in id 1
"#;
        let mut vm = VM::new();
        let result = run_expr(&mut vm, text);
        assert_eq!(result, Int(0));
    }

    #[test]
    fn run_expr_int() {
        let _ = ::env_logger::init();
        let text = r#"io.run_expr "123" "#;
        let mut vm = make_vm();
        let result = run_expr(&mut vm, text);
        assert_eq!(result, Value::String(vm.gc.borrow_mut().alloc("123")));
    }

    test_expr!{ run_expr_io,
r#"io_bind (io.run_expr "io.print_int 123") (\x -> io_return 100) "#,
Int(100)
}

    #[test]
    fn rename_types_after_binding() {
        let _ = ::env_logger::init();
        let text = r#"
let prelude = import "std/prelude.hs"
in
let { List } = prelude
and { (==) }: Eq (List Int) = prelude.eq_List { (==) }
in Cons 1 Nil == Nil
"#;
        let mut vm = make_vm();
        let value = super::run_expr(&mut vm, "<top>", text).unwrap_or_else(|err| panic!("{}", err));
        assert_eq!(value.value, Int(0));
    }

    #[test]
    fn test_implicit_prelude() {
        let _ = ::env_logger::init();
        let text = r#"Ok (Some (1.0 + 3.0 - 2.0)) "#;
        let mut vm = make_vm();
        super::run_expr(&mut vm, "<top>", text).unwrap_or_else(|err| panic!("{}", err));
    }

    /// Creates a VM for testing which has the correct paths to import the std library properly
    fn make_vm<'a>() -> VM<'a> {
        let vm = VM::new();
        let import_symbol = vm.symbol("import");
        let import = vm.macros.get(import_symbol);
        import.as_ref()
              .and_then(|import| import.downcast_ref::<::import::Import>())
              .expect("Import macro")
              .add_path("..");
        vm
    }

    #[test]
    fn test_prelude() {
        let _ = ::env_logger::init();
        let vm = make_vm();
        run_expr(&vm, r#" import "std/prelude.hs" "#);
    }

    #[test]
    fn value_size() {
        assert!(::std::mem::size_of::<Value>() <= 16);
    }
}
