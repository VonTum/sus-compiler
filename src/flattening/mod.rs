
pub mod name_context;

use std::{ops::Deref, iter::zip};

use crate::{
    arena_alloc::{ArenaAllocator, FlatAlloc, UUIDMarker, UUIDRange, UUID}, ast::{AssignableExpressionModifiers, CodeBlock, Expression, Identifier, IdentifierType, InterfacePorts, LeftExpression, Module, Operator, SignalDeclaration, SpanExpression, SpanTypeExpression, Statement, TypeExpression}, errors::{error_info, ErrorCollector, ErrorInfo}, file_position::{BracketSpan, Span}, linker::{ConstantUUID, FileUUID, GlobalResolver, Linker, ModuleUUID, NameElem, NamedConstant, NamedType, ResolvedGlobals, ResolvedNameElem, TypeUUIDMarker}, tokenizer::TOKEN_IDENTIFIER, typing::{get_binary_operator_types, typecheck, typecheck_is_array_indexer, typecheck_unary_operator, Type, WrittenType, BOOL_TYPE, INT_TYPE}, value::Value
};

use self::name_context::LocalVariableContext;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub struct FlatIDMarker;
impl UUIDMarker for FlatIDMarker {const DISPLAY_NAME : &'static str = "obj_";}
pub type FlatID = UUID<FlatIDMarker>;

pub type FlatIDRange = UUIDRange<FlatIDMarker>;

#[derive(Debug)]
pub enum ConnectionWritePathElement {
    ArrayIdx{idx : FlatID, bracket_span : BracketSpan},
    //StructField(FieldID)
}
#[derive(Debug)]
pub enum ConnectionWritePathElementComputed {
    ArrayIdx(usize)
}

// These are assignable connections
#[derive(Debug)]
pub struct ConnectionWrite {
    pub root : FlatID,
    pub root_span : Span,
    pub path : Vec<ConnectionWritePathElement>,
    pub span : Span,
    pub is_declared_in_this_module : bool
}

#[derive(Debug)]
pub enum WriteType {
    Connection{num_regs : i64, regs_span : Option<Span>},
    Initial
}

#[derive(Debug)]
pub struct Write {
    pub write_type : WriteType,
    pub from : FlatID,
    pub to : ConnectionWrite
}

#[derive(Debug)]
pub enum WireSource {
    WireRead(FlatID), // Used to add a span to the reference of a wire. 
    UnaryOp{op : Operator, right : FlatID},
    BinaryOp{op : Operator, left : FlatID, right : FlatID},
    ArrayAccess{arr : FlatID, arr_idx : FlatID},
    Constant(Value),
    NamedConstant(ConstantUUID),
}

impl WireSource {
    pub fn for_each_input_wire<F : FnMut(FlatID)>(&self, func : &mut F) {
        match self {
            &WireSource::WireRead(from_wire) => {func(from_wire)}
            &WireSource::UnaryOp { op:_, right } => {func(right)}
            &WireSource::BinaryOp { op:_, left, right } => {func(left); func(right)}
            &WireSource::ArrayAccess { arr, arr_idx } => {func(arr); func(arr_idx)}
            WireSource::Constant(_) => {}
            WireSource::NamedConstant(_) => {}
        }
    }
}

const IS_GEN_UNINIT : bool = false;

#[derive(Debug)]
pub struct WireInstance {
    pub typ : Type,
    pub is_compiletime : bool,
    pub span : Span,
    pub is_declared_in_this_module : bool,
    pub source : WireSource
}

#[derive(Debug)]
pub struct Declaration {
    pub typ_expr : WrittenType,
    pub typ : Type,
    pub is_declared_in_this_module : bool,
    pub name_token : usize,
    pub name : Box<str>,
    pub read_only : bool,
    // If the program text already covers the write, then lsp stuff on this declaration shouldn't use it. 
    pub is_free_standing_decl : bool,
    pub identifier_type : IdentifierType,
    pub latency_specifier : Option<FlatID>
}

impl Declaration {
    pub fn make_declared_here(&self, file : FileUUID) -> ErrorInfo {
        error_info(Span::new_extend_to_include_token(self.typ_expr.get_span(), self.name_token), file, "Declared here")
    }
}

#[derive(Debug)]
pub struct SubModuleInstance {
    pub module_uuid : ModuleUUID,
    pub name : Box<str>,
    pub module_name_span : Span,
    pub is_declared_in_this_module : bool,
    pub interface_ports : InterfacePorts<FlatID>
}

#[derive(Debug)]
pub struct IfStatement {
    pub condition : FlatID,
    pub then_start : FlatID,
    pub then_end_else_start : FlatID,
    pub else_end : FlatID
}

#[derive(Debug)]
// Always is_compiletime
pub struct ForStatement {
    pub loop_var_decl : FlatID,
    pub start : FlatID,
    pub end : FlatID,
    pub loop_body : FlatIDRange
}

#[derive(Debug)]
pub enum Instruction {
    SubModule(SubModuleInstance),
    Declaration(Declaration),
    Wire(WireInstance),
    Write(Write),
    IfStatement(IfStatement),
    ForStatement(ForStatement)
}

impl Instruction {
    #[track_caller]
    pub fn extract_wire(&self) -> &WireInstance {
        let Self::Wire(w) = self else {panic!("extract_wire on not a wire! Found {self:?}")};
        w
    }
    #[track_caller]
    pub fn extract_wire_declaration(&self) -> &Declaration {
        let Self::Declaration(w) = self else {panic!("extract_wire on not a WireDeclaration! Found {self:?}")};
        w
    }
    #[track_caller]
    pub fn extract_submodule(&self) -> &SubModuleInstance {
        let Self::SubModule(sm) = self else {panic!("extract_wire on not a SubModule! Found {self:?}")};
        sm
    }
    #[track_caller]
    pub fn extract_write(&self) -> &Write {
        let Self::Write(sm) = self else {panic!("extract_write on not a Write! Found {self:?}")};
        sm
    }

    pub fn for_each_embedded_type<F : FnMut(&Type, Span)>(&self, f : &mut F) {
        match self {
            Instruction::SubModule(_) | Instruction::Write(_) | Instruction::IfStatement(_) | Instruction::ForStatement(_) => {}
            Instruction::Declaration(decl) => {
                f(&decl.typ, decl.typ_expr.get_span());
            }
            Instruction::Wire(w) => {
                f(&w.typ, w.span);
            }
        }
    }

    pub fn get_location_of_module_part(&self) -> Option<Span> {
        match self {
            Instruction::SubModule(sm) => sm.is_declared_in_this_module.then_some(sm.module_name_span),
            Instruction::Declaration(decl) => decl.is_declared_in_this_module.then_some(Span::new_single_token(decl.name_token)),
            Instruction::Wire(w) => w.is_declared_in_this_module.then_some(w.span),
            Instruction::Write(conn) => conn.to.is_declared_in_this_module.then_some(conn.to.span),
            Instruction::IfStatement(_) | Instruction::ForStatement(_) => None
        }
    }
}

enum LocalOrGlobal<'l, 'e> {
    Local(FlatID),
    Global(ResolvedNameElem<'l, 'e>)
}

impl<'l, 'e> LocalOrGlobal<'l, 'e> {
    fn expect_local(self, context : &str) -> Option<FlatID> {
        match self {
            LocalOrGlobal::Local(local) => Some(local),
            LocalOrGlobal::Global(global) => {
                global.errors.error_basic(global.span, format!("Can only use local variables in {context}!"));
                None
            }
        }
    }
}

struct FlatteningContext<'prev, 'inst, 'l, 'runtime> {
    instructions : &'inst mut FlatAlloc<Instruction, FlatIDMarker>,
    errors : &'runtime ErrorCollector,
    is_declared_in_this_module : bool,

    local_variable_context : LocalVariableContext<'prev, 'l, FlatID>,
    linker : &'runtime GlobalResolver<'l>,
    pub type_list_for_naming : &'l ArenaAllocator<NamedType, TypeUUIDMarker>,
    module : &'l Module,
}

impl<'prev, 'inst, 'l, 'runtime> FlatteningContext<'prev, 'inst, 'l, 'runtime> {
    fn map_to_type(&mut self, type_expr : &SpanTypeExpression) -> WrittenType {
        match &type_expr.0 {
            TypeExpression::Named => {
                if let Some(typ_id) = &self.linker.resolve_global(type_expr.1, &self.errors).expect_type() {
                    WrittenType::Named(type_expr.1, *typ_id)
                } else {
                    WrittenType::Error(type_expr.1)
                }
            }
            TypeExpression::Array(b) => {
                let (array_type_expr, array_size_expr) = b.deref();
                let array_element_type = self.map_to_type(&array_type_expr);
                let array_size_wire_id = self.flatten_expr(array_size_expr);
                WrittenType::Array(type_expr.1, Box::new((array_element_type, array_size_wire_id)))
            }
        }
    }
    fn flatten_declaration<const ALLOW_MODULES : bool>(&mut self, decl : &'l SignalDeclaration, read_only : bool, is_free_standing_decl : bool) -> FlatID {
        let typ_expr = if let TypeExpression::Named = &decl.typ.0 {
            let resolved = self.linker.resolve_global(decl.typ.1, &self.errors);
            match resolved.name_elem {
                Some(NameElem::Module(id)) if ALLOW_MODULES => {
                    let md = &self.linker.get_module(id);
                    return self.alloc_module_interface(decl.name.clone(), md, id, decl.typ.1)
                }
                Some(NameElem::Type(id)) => {
                    WrittenType::Named(decl.typ.1, id)
                }
                Some(_global_module_or_type) => {
                    let accepted = if ALLOW_MODULES {"Type or Module"} else {"Type"};
                    resolved.not_expected_global_error(accepted);
                    WrittenType::Error(decl.typ.1)
                }
                None => WrittenType::Error(decl.typ.1)
            }
        } else {
            self.map_to_type(&decl.typ)
        };

        let latency_specifier = if let Some(lat_expr) = &decl.latency_expr {
            let latency_spec = self.flatten_expr(lat_expr);
            Some(latency_spec)
        } else {
            None
        };

        let typ_expr_span = typ_expr.get_span();
        let inst_id = self.instructions.alloc(Instruction::Declaration(Declaration{
            typ : typ_expr.to_type(),
            typ_expr,
            is_declared_in_this_module : self.is_declared_in_this_module,
            read_only,
            is_free_standing_decl,
            identifier_type : decl.identifier_type,
            name : decl.name.clone(),
            name_token : decl.name_token,
            latency_specifier
        }));

        if let Err(conflict) = self.local_variable_context.add_declaration(&decl.name, inst_id) {
            self.errors.error_with_info(Span::new_extend_to_include_token(typ_expr_span, decl.name_token), "This declaration conflicts with a previous declaration in the same scope", vec![self.instructions[conflict].extract_wire_declaration().make_declared_here(self.errors.file)])
        }

        inst_id
    }
    fn resolve_identifier(&self, identifier : &Identifier) -> LocalOrGlobal {
        // Possibly local
        if let Some(single_tok_idx) = identifier.span.is_single_token() {
            assert!(self.linker.file.tokens[single_tok_idx] == TOKEN_IDENTIFIER);
            if let Some(decl_id) = self.local_variable_context.get_declaration_for(self.linker.file.get_token_text(single_tok_idx)) {
                return LocalOrGlobal::Local(decl_id);
            }
        }
        // Global identifier
        LocalOrGlobal::Global(self.linker.resolve_global(identifier.span, &self.errors))
    }
    fn initialize_interface<const IS_SUBMODULE : bool>(&mut self) -> InterfacePorts<FlatID> {
        let ports : Box<[FlatID]> = self.module.interface.ports.iter().enumerate().map(|(idx, port_decl)|{
            let is_input = idx < self.module.interface.outputs_start;
            let read_only = is_input ^ IS_SUBMODULE;

            self.flatten_declaration::<false>(port_decl, read_only, true)
        }).collect();
        InterfacePorts{ports, outputs_start : self.module.interface.outputs_start}
    }
    fn alloc_module_interface(&mut self, name : Box<str>, module : &Module, module_uuid : ModuleUUID, typ_span : Span) -> FlatID {
        let unused_errors = ErrorCollector::new(module.link_info.file); // Temporary ErrorCollector, unused
        let local_linker = self.linker.new_sublinker(module.link_info.file);
        let mut nested_context = FlatteningContext {
            instructions: self.instructions,
            errors: &unused_errors,
            is_declared_in_this_module: false,
            linker: &local_linker,
            type_list_for_naming: self.type_list_for_naming,
            local_variable_context : LocalVariableContext::new_initial(),
            module,
        };
        
        let interface_ports = nested_context.initialize_interface::<true>();
        
        self.linker.reabsorb_sublinker(local_linker);

        self.instructions.alloc(Instruction::SubModule(SubModuleInstance{
            name,
            module_uuid,
            is_declared_in_this_module : self.is_declared_in_this_module,
            module_name_span: typ_span,
            interface_ports
        }))
    }
    // Returns the module, full interface, and the output range for the function call syntax
    fn desugar_func_call(&mut self, func_and_args : &[SpanExpression], func_call_span : Span) -> Option<(&Module, InterfacePorts<FlatID>)> {
        let (name_expr, name_expr_span) = &func_and_args[0]; // Function name is always there
        let Expression::Named(name) = name_expr else {
            self.errors.error_basic(*name_expr_span, "Function call name must be a simple identifier");
            return None;
        };
        let func_instantiation_id = match self.resolve_identifier(name) {
            LocalOrGlobal::Local(id) => id,
            LocalOrGlobal::Global(global) => {
                let module_id = global.expect_module()?;
                let md = &self.linker.get_module(module_id);
                self.alloc_module_interface(md.link_info.name.clone(), md, module_id, *name_expr_span)
            }
        };

        let func_instantiation = &self.instructions[func_instantiation_id].extract_submodule();
        let md = &self.linker.get_module(func_instantiation.module_uuid);

        let submodule_local_wires = func_instantiation.interface_ports.clone();
        
        let inputs = submodule_local_wires.func_call_syntax_inputs();

        let mut args = &func_and_args[1..];

        let arg_count = args.len();
        let expected_arg_count = inputs.len();
        if arg_count != expected_arg_count {
            let module_info = vec![error_info(md.link_info.span, md.link_info.file, "Interface defined here")];
            if arg_count > expected_arg_count {
                // Too many args, complain about excess args at the end
                let excess_args_span = Span::new_overarching(args[expected_arg_count].1, func_call_span).dont_include_last_token();
                self.errors.error_with_info(excess_args_span, format!("Excess argument. Function takes {expected_arg_count} args, but {arg_count} were passed."), module_info);
                // Shorten args to still get proper type checking for smaller arg array
                args = &args[..expected_arg_count];
            } else {
                // Too few args, mention missing argument names
                self.errors.error_with_info(func_call_span.only_last_token(), format!("Too few arguments. Function takes {expected_arg_count} args, but {arg_count} were passed."), module_info);
            }
        }

        for (field, arg_expr) in zip(inputs, args) {
            let arg_read_side = self.flatten_expr(arg_expr);
            let func_input_port = &submodule_local_wires.ports[field];
            self.instructions.alloc(Instruction::Write(Write{write_type : WriteType::Connection{num_regs : 0, regs_span : None}, from: arg_read_side, to: ConnectionWrite{root : *func_input_port, root_span : arg_expr.1, path : Vec::new(), span : *name_expr_span, is_declared_in_this_module : self.is_declared_in_this_module}}));
        }

        Some((md, submodule_local_wires))
    }
    fn flatten_expr(&mut self, (expr, expr_span) : &SpanExpression) -> FlatID {
        let source = match expr {
            Expression::Named(name) => {
                match self.resolve_identifier(name) {
                    LocalOrGlobal::Local(id) => {
                        WireSource::WireRead(id)
                    }
                    LocalOrGlobal::Global(global) => {
                        if let Some(cst) = global.expect_constant() {
                            WireSource::NamedConstant(cst)
                        } else {
                            WireSource::Constant(Value::Error)
                        }
                    }
                }
            }
            Expression::Constant(cst) => {
                WireSource::Constant(cst.clone())
            }
            Expression::UnaryOp(op_box) => {
                let (op, _op_pos, operate_on) = op_box.deref();
                let right = self.flatten_expr(operate_on);
                WireSource::UnaryOp{op : *op, right}
            }
            Expression::BinOp(binop_box) => {
                let (left_expr, op, _op_pos, right_expr) = binop_box.deref();
                let left = self.flatten_expr(left_expr);
                let right = self.flatten_expr(right_expr);
                WireSource::BinaryOp{op : *op, left, right}
            }
            Expression::Array(arr_box) => {
                let (left, right, _bracket_span) = arr_box.deref();
                let arr = self.flatten_expr(left);
                let arr_idx = self.flatten_expr(right);
                WireSource::ArrayAccess{arr, arr_idx}
            }
            Expression::FuncCall(func_and_args) => {
                if let Some((md, interface_wires)) = self.desugar_func_call(func_and_args, *expr_span) {
                    let output_range = interface_wires.func_call_syntax_outputs();

                    if output_range.len() != 1 {
                        let info = error_info(md.link_info.span, md.link_info.file, "Module Defined here");
                        self.errors.error_with_info(*expr_span, "A function called in this context may only return one result. Split this function call into a separate line instead.", vec![info]);
                    }

                    if output_range.len() >= 1 {
                        return interface_wires.ports[output_range.start];
                    }
                }
                // Function desugaring or using threw an error
                WireSource::Constant(Value::Error)
            }
        };

        let wire_instance = WireInstance{
            typ : Type::Unknown,
            is_compiletime : IS_GEN_UNINIT,
            span : *expr_span,
            source,
            is_declared_in_this_module : self.is_declared_in_this_module
        };
        self.instructions.alloc(Instruction::Wire(wire_instance))
    }
    fn flatten_assignable_expr(&mut self, expr : &Expression, span : Span) -> Option<ConnectionWrite> {
        match expr {
            Expression::Named(local_idx) => {
                let root = self.resolve_identifier(local_idx).expect_local("assignments")?;

                Some(ConnectionWrite{root, root_span : span, path : Vec::new(), span, is_declared_in_this_module : self.is_declared_in_this_module})
            }
            Expression::Array(arr_box) => {
                let (arr, idx_expr, bracket_span) = arr_box.deref();
                let flattened_arr_expr_opt = self.flatten_assignable_expr(&arr.0, arr.1);
                
                let idx = self.flatten_expr(idx_expr);

                let mut flattened_arr_expr = flattened_arr_expr_opt?; // only unpack the subexpr after flattening the idx, so we catch all errors

                flattened_arr_expr.path.push(ConnectionWritePathElement::ArrayIdx{idx, bracket_span : *bracket_span});
                flattened_arr_expr.span = Span::new_overarching(flattened_arr_expr.span, bracket_span.outer_span());
                Some(flattened_arr_expr)
            }
            Expression::Constant(_) => {self.errors.error_basic(span, "Cannot assign to constant"); None},
            Expression::UnaryOp(_) => {self.errors.error_basic(span, "Cannot assign to the result of an operator"); None},
            Expression::BinOp(_) => {self.errors.error_basic(span, "Cannot assign to the result of an operator"); None},
            Expression::FuncCall(_) => {self.errors.error_basic(span, "Cannot assign to submodule call"); None},
        }
    }
    fn flatten_left_expr(&mut self, left : &'l LeftExpression, span : Span, gets_assigned : bool) -> Option<ConnectionWrite> {
        match left {
            LeftExpression::Assignable(assignable) => {
                self.flatten_assignable_expr(assignable, span)
            }
            LeftExpression::Declaration(decl) => {
                let root = self.flatten_declaration::<true>(decl, false, !gets_assigned);
                Some(ConnectionWrite{root, root_span : Span::new_single_token(decl.name_token), path: Vec::new(), span, is_declared_in_this_module: true})
            }
        }
    }

    fn flatten_assignment_modifiers(&mut self, modifiers : &AssignableExpressionModifiers) -> WriteType {
        match modifiers {
            &AssignableExpressionModifiers::LatencyAdding{num_regs, regs_span} => WriteType::Connection{num_regs, regs_span : Some(regs_span)},
            AssignableExpressionModifiers::Initial{initial_token : _} => WriteType::Initial,
            AssignableExpressionModifiers::NoModifiers => WriteType::Connection{num_regs : 0, regs_span : None},
        }
    }
    fn flatten_code(&mut self, code : &'l CodeBlock) {
        let mut inner_context = FlatteningContext{
            instructions: self.instructions,
            errors: self.errors,
            is_declared_in_this_module: self.is_declared_in_this_module,
            local_variable_context: self.local_variable_context.extend(),
            linker: self.linker,
            type_list_for_naming: self.type_list_for_naming,
            module: self.module
        };
        inner_context.flatten_code_keep_context(code);
    }
    fn flatten_code_keep_context(&mut self, code : &'l CodeBlock) {
        for (stmt, stmt_span) in &code.statements {
            match stmt {
                Statement::Assign{to, expr : Some((Expression::FuncCall(func_and_args), func_span)), eq_sign_position} => {
                    let Some((md, interface)) = self.desugar_func_call(&func_and_args, *func_span) else {continue};
                    let output_range = interface.func_call_syntax_outputs();
                    let outputs = &interface.ports[output_range];

                    let func_name_span = func_and_args[0].1;
                    let num_func_outputs = outputs.len();
                    let num_targets = to.len();
                    if num_targets != num_func_outputs {
                        let info = vec![error_info(md.link_info.span, md.link_info.file, "Module Defined here")];
                        if num_targets > num_func_outputs {
                            let excess_results_span = Span::new_overarching(to[num_func_outputs].span, *func_span).dont_include_last_token();
                            self.errors.error_with_info(excess_results_span, format!("Excess output targets. Function returns {num_func_outputs} results, but {num_targets} targets were given."), info);
                        } else {
                            let too_few_targets_pos = if let Some(eq) = eq_sign_position {Span::new_single_token(*eq)} else {func_name_span};
                            self.errors.error_with_info(too_few_targets_pos, format!("Too few output targets. Function returns {num_func_outputs} results, but {num_targets} targets were given."), info);
                        }
                    }

                    for (field, to_i) in zip(outputs, to) {                        
                        let module_port_wire_decl = self.instructions[*field].extract_wire_declaration();
                        let module_port_proxy = self.instructions.alloc(Instruction::Wire(WireInstance{typ : module_port_wire_decl.typ.clone(), is_compiletime : IS_GEN_UNINIT, span : *func_span, is_declared_in_this_module : self.is_declared_in_this_module, source : WireSource::WireRead(*field)}));
                        let Some(write_side) = self.flatten_left_expr(&to_i.expr, to_i.span, true) else {continue};

                        let write_type = self.flatten_assignment_modifiers(&to_i.modifiers);
                        self.instructions.alloc(Instruction::Write(Write{write_type, from: module_port_proxy, to: write_side}));
                    }
                },
                Statement::Assign{to, expr : non_func_expr, eq_sign_position : _} => {
                    let read_side = non_func_expr.as_ref().map(|some_expr| self.flatten_expr(some_expr));
                    if to.len() == 1 {
                        let t = &to[0];
                        let Some(write_side) = self.flatten_left_expr(&t.expr, t.span, non_func_expr.is_some()) else {continue};
                        let write_type = self.flatten_assignment_modifiers(&t.modifiers);
                        if let Some(read_side) = read_side {
                            self.instructions.alloc(Instruction::Write(Write{write_type, from: read_side, to: write_side}));
                        }
                    } else {
                        self.errors.error_basic(*stmt_span, format!("Non-function assignments must only output exactly 1 instead of {}", to.len()));
                    }
                },
                Statement::Block(inner_code) => {
                    self.flatten_code(inner_code);
                },
                Statement::If{condition : condition_expr, then, els} => {
                    let condition = self.flatten_expr(condition_expr);

                    let if_id = self.instructions.alloc(Instruction::IfStatement(IfStatement{condition, then_start : UUID::PLACEHOLDER, then_end_else_start : UUID::PLACEHOLDER, else_end : UUID::PLACEHOLDER}));
                    let then_start = self.instructions.get_next_alloc_id();

                    self.flatten_code(then);
                    let then_end_else_start = self.instructions.get_next_alloc_id();
                    if let Some(e) = els {
                        self.flatten_code(e);
                    }
                    let else_end = self.instructions.get_next_alloc_id();

                    let Instruction::IfStatement(if_stmt) = &mut self.instructions[if_id] else {unreachable!()};
                    if_stmt.then_start = then_start;
                    if_stmt.then_end_else_start = then_end_else_start;
                    if_stmt.else_end = else_end;
                }
                Statement::For{var, range, code} => {
                    let loop_var_decl = self.flatten_declaration::<false>(var, true, true);

                    let start = self.flatten_expr(&range.from);
                    let end = self.flatten_expr(&range.to);
                    
                    let for_id = self.instructions.alloc(Instruction::ForStatement(ForStatement{loop_var_decl, start, end, loop_body: UUIDRange(UUID::PLACEHOLDER, UUID::PLACEHOLDER)}));

                    let code_start = self.instructions.get_next_alloc_id();

                    self.flatten_code(code);
                    
                    let code_end = self.instructions.get_next_alloc_id();

                    let Instruction::ForStatement(for_stmt) = &mut self.instructions[for_id] else {unreachable!()};

                    for_stmt.loop_body = UUIDRange(code_start, code_end);
                }
            }
        }
    }

    /*
        ==== Typechecking ====
    */
    fn typecheck_wire_is_of_type(&self, wire : &WireInstance, expected : &Type, context : &str) {
        typecheck(&wire.typ, wire.span, expected, context, self.type_list_for_naming, &self.errors);
    }

    fn typecheck_connection(&self, to : &ConnectionWrite, from : FlatID) {
        // Typecheck digging down into write side
        let conn_root = self.instructions[to.root].extract_wire_declaration();
        let mut write_to_type = Some(&conn_root.typ);
        for p in &to.path {
            match p {
                &ConnectionWritePathElement::ArrayIdx{idx, bracket_span} => {
                    let idx_wire = self.instructions[idx].extract_wire();
                    self.typecheck_wire_is_of_type(idx_wire, &INT_TYPE, "array index");
                    if let Some(wr) = write_to_type {
                        write_to_type = typecheck_is_array_indexer(wr, bracket_span.outer_span(), self.type_list_for_naming, &self.errors);
                    }
                }
            }
        }

        // Typecheck the value with target type
        let from_wire = self.instructions[from].extract_wire();
        if let Some(target_type) = write_to_type {
            self.typecheck_wire_is_of_type(from_wire, &target_type, "connection");
        }
    }

    fn typecheck(&mut self) {
        let look_at_queue : Vec<FlatID> = self.instructions.iter().map(|(id,_)| id).collect();

        for elem_id in look_at_queue {
            match &self.instructions[elem_id] {
                Instruction::SubModule(_) => {}
                Instruction::Declaration(decl) => {
                    if let Some(latency_spec) = decl.latency_specifier {
                        let latency_spec_wire = &self.instructions[latency_spec].extract_wire();
                        self.typecheck_wire_is_of_type(latency_spec_wire, &INT_TYPE, "latency specifier");
                    }

                    decl.typ.for_each_generative_input(&mut |param_id| {
                        self.typecheck_wire_is_of_type(self.instructions[param_id].extract_wire(), &INT_TYPE, "Array size");
                    });
                }
                Instruction::IfStatement(stm) => {
                    let wire = &self.instructions[stm.condition].extract_wire();
                    self.typecheck_wire_is_of_type(wire, &BOOL_TYPE, "if statement condition")
                }
                Instruction::ForStatement(stm) => {
                    let loop_var = &self.instructions[stm.loop_var_decl].extract_wire_declaration();
                    let start = &self.instructions[stm.start].extract_wire();
                    let end = &self.instructions[stm.end].extract_wire();
                    self.typecheck_wire_is_of_type(start, &loop_var.typ, "for loop");
                    self.typecheck_wire_is_of_type(end, &loop_var.typ, "for loop");
                }
                Instruction::Wire(w) => {
                    let result_typ = match &w.source {
                        &WireSource::WireRead(from_wire) => {
                            self.instructions[from_wire].extract_wire_declaration().typ.clone()
                        }
                        &WireSource::UnaryOp{op, right} => {
                            let right_wire = self.instructions[right].extract_wire();
                            typecheck_unary_operator(op, &right_wire.typ, right_wire.span, self.type_list_for_naming, &self.errors)
                        }
                        &WireSource::BinaryOp{op, left, right} => {
                            let left_wire = self.instructions[left].extract_wire();
                            let right_wire = self.instructions[right].extract_wire();
                            let ((input_left_type, input_right_type), output_type) = get_binary_operator_types(op);
                            self.typecheck_wire_is_of_type(left_wire, &input_left_type, &format!("{op} left"));
                            self.typecheck_wire_is_of_type(right_wire, &input_right_type, &format!("{op} right"));
                            output_type
                        }
                        &WireSource::ArrayAccess{arr, arr_idx} => {
                            let arr_wire = self.instructions[arr].extract_wire();
                            let arr_idx_wire = self.instructions[arr_idx].extract_wire();
                
                            self.typecheck_wire_is_of_type(arr_idx_wire, &INT_TYPE, "array index");
                            if let Some(typ) = typecheck_is_array_indexer(&arr_wire.typ, arr_wire.span, self.type_list_for_naming, &self.errors) {
                                typ.clone()
                            } else {
                                Type::Error
                            }
                        }
                        WireSource::Constant(value) => {
                            value.get_type_of_constant()
                        }
                        &WireSource::NamedConstant(id) => {
                            let NamedConstant::Builtin{name:_, typ, val:_} = &self.linker.get_constant(id);
                            typ.clone()
                        }
                    };
                    let Instruction::Wire(w) = &mut self.instructions[elem_id] else {unreachable!()};
                    w.typ = result_typ;
                }
                Instruction::Write(conn) => {
                    self.typecheck_connection(&conn.to, conn.from);
                }
            }
        }

        // Post type application. Flag any remaining Type::Unknown
        for (_id, inst) in self.instructions.iter() {
            inst.for_each_embedded_type(&mut |typ, span| {
                if typ.contains_error_or_unknown::<false, true>() {
                    self.errors.error_basic(span, format!("Unresolved Type: {}", typ.to_string(self.type_list_for_naming)))
                }
            });
        }
    }

    /*
        ==== Generative Code Checking ====
    */
    fn must_be_compiletime_with_info<CtxFunc : FnOnce() -> Vec<ErrorInfo>>(&self, wire : &WireInstance, context : &str, ctx_func : CtxFunc) {
        if !wire.is_compiletime {
            self.errors.error_with_info(wire.span, format!("{context} must be compile time"), ctx_func());
        }
    }
    fn must_be_compiletime(&self, wire : &WireInstance, context : &str) {
        self.must_be_compiletime_with_info(wire, context, || Vec::new());
    }

    fn generative_check(&mut self) {
        let mut runtime_if_stack : Vec<(FlatID, Span)> = Vec::new();

        let mut declaration_depths : FlatAlloc<Option<usize>, FlatIDMarker> = self.instructions.iter().map(|_| None).collect();

        for inst_id in self.instructions.id_range() {
            while let Some((end_id, span)) = runtime_if_stack.pop() {
                if end_id != inst_id {
                    runtime_if_stack.push((end_id, span));
                    break;
                }
            }
            match &self.instructions[inst_id] {
                Instruction::SubModule(_) => {}
                Instruction::Declaration(decl) => {
                    if decl.identifier_type.is_generative() {
                        assert!(declaration_depths[inst_id].is_none());
                        declaration_depths[inst_id] = Some(runtime_if_stack.len())
                    }

                    if let Some(latency_specifier) = decl.latency_specifier {
                        self.must_be_compiletime(self.instructions[latency_specifier].extract_wire(), "Latency specifier");
                    }

                    decl.typ.for_each_generative_input(&mut |param_id| {
                        self.must_be_compiletime(self.instructions[param_id].extract_wire(), "Array size");
                    });
                }
                Instruction::Wire(wire) => {
                    let mut is_generative = true;
                    if let WireSource::WireRead(from) = &wire.source {
                        let decl = self.instructions[*from].extract_wire_declaration();
                        if !decl.identifier_type.is_generative() {
                            is_generative = false;
                        }
                    } else {
                        wire.source.for_each_input_wire(&mut |source_id| {
                            let source_wire = self.instructions[source_id].extract_wire();
                            if !source_wire.is_compiletime {
                                is_generative = false;
                            }
                        });
                    }
                    let Instruction::Wire(wire) = &mut self.instructions[inst_id] else {unreachable!()};
                    wire.is_compiletime = is_generative;
                }
                Instruction::Write(conn) => {
                    let decl = self.instructions[conn.to.root].extract_wire_declaration();

                    if decl.read_only {
                        self.errors.error_with_info(conn.to.span, "Cannot Assign to Read-Only value", vec![decl.make_declared_here(self.errors.file)]);
                    }

                    let from_wire = self.instructions[conn.from].extract_wire();
                    match conn.write_type {
                        WriteType::Connection{num_regs : _, regs_span : _} => {
                            if decl.identifier_type.is_generative() {
                                // Check that whatever's written to this declaration is also generative
                                self.must_be_compiletime_with_info(from_wire, "Assignments to generative variables", || vec![decl.make_declared_here(self.errors.file)]);

                                // Check that this declaration isn't used in a non-compiletime if
                                let declared_at_depth = declaration_depths[conn.to.root].unwrap();
            
                                if runtime_if_stack.len() > declared_at_depth {
                                    let mut infos = Vec::new();
                                    infos.push(decl.make_declared_here(self.errors.file));
                                    for (_, if_cond_span) in &runtime_if_stack[declared_at_depth..] {
                                        infos.push(error_info(*if_cond_span, self.errors.file, "Runtime Condition here"));
                                    }
                                    self.errors.error_with_info(conn.to.span, "Cannot write to generative variables in runtime conditional block", infos);
                                }
                            }
                        }
                        WriteType::Initial => {
                            if decl.identifier_type != IdentifierType::State {
                                self.errors.error_with_info(conn.to.span, "Initial values can only be given to state registers!", vec![decl.make_declared_here(self.errors.file)])
                            }
                            self.must_be_compiletime(from_wire, "initial value assignment")
                        }
                    }
                }
                Instruction::IfStatement(if_stmt) => {
                    let condition_wire = self.instructions[if_stmt.condition].extract_wire();
                    if !condition_wire.is_compiletime {
                        runtime_if_stack.push((if_stmt.else_end, condition_wire.span));
                    }
                }
                Instruction::ForStatement(_) => {}
            }
        }
    }

    /* 
        ==== Additional Warnings ====
    */
    fn find_unused_variables(&self, interface : &InterfacePorts<FlatID>) {
        // Setup Wire Fanouts List for faster processing
        let mut instance_fanins : FlatAlloc<Vec<FlatID>, FlatIDMarker> = self.instructions.iter().map(|_| Vec::new()).collect();

        for (inst_id, inst) in self.instructions.iter() {
            match inst {
                Instruction::Write(conn) => {
                    instance_fanins[conn.to.root].push(conn.from);
                }
                Instruction::SubModule(sm) => {
                    for w in sm.interface_ports.outputs() {
                        instance_fanins[*w].push(inst_id);
                    }
                    for port in sm.interface_ports.inputs() {
                        instance_fanins[inst_id].push(*port);
                    }
                }
                Instruction::Declaration(decl) => {
                    decl.typ.for_each_generative_input(&mut |id| instance_fanins[inst_id].push(id));
                }
                Instruction::Wire(wire) => {
                    wire.source.for_each_input_wire(&mut |id| instance_fanins[inst_id].push(id));
                }
                Instruction::IfStatement(stm) => {
                    for id in UUIDRange(stm.then_start, stm.else_end) {
                        if let Instruction::Write(conn) = &self.instructions[id] {
                            instance_fanins[conn.to.root].push(stm.condition);
                        }
                    }
                }
                Instruction::ForStatement(stm) => {
                    instance_fanins[stm.loop_var_decl].push(stm.start);
                    instance_fanins[stm.loop_var_decl].push(stm.end);
                }
            }
        }

        let mut is_instance_used_map : FlatAlloc<bool, FlatIDMarker> = self.instructions.iter().map(|_| false).collect();

        let mut wire_to_explore_queue : Vec<FlatID> = Vec::new();

        for port in interface.outputs() {
            is_instance_used_map[*port] = true;
            wire_to_explore_queue.push(*port);
        }

        while let Some(item) = wire_to_explore_queue.pop() {
            for from in &instance_fanins[item] {
                if !is_instance_used_map[*from] {
                    is_instance_used_map[*from] = true;
                    wire_to_explore_queue.push(*from);
                }
            }
        }

        // Now produce warnings from the unused list
        for (id, inst) in self.instructions.iter() {
            if !is_instance_used_map[id] {
                if let Instruction::Declaration(decl) = inst {
                    if decl.is_declared_in_this_module {
                        self.errors.warn_basic(Span::new_single_token(decl.name_token), "Unused Variable: This variable does not affect the output ports of this module");
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct FlattenedInterfacePort {
    pub wire_id : FlatID,
    pub port_name : Box<str>,
    pub span : Span
}

#[derive(Debug)]
pub struct FlattenedModule {
    pub instructions : FlatAlloc<Instruction, FlatIDMarker>,
    pub errors : ErrorCollector,
    pub interface_ports : InterfacePorts<FlatID>,
    pub resolved_globals : ResolvedGlobals
}

impl FlattenedModule {
    pub fn empty(file : FileUUID) -> FlattenedModule {
        FlattenedModule {
            instructions : FlatAlloc::new(),
            errors : ErrorCollector::new(file),
            interface_ports : InterfacePorts::empty(),
            resolved_globals : ResolvedGlobals::new()
        }
    }
    
    /*
    This method flattens all given code into a simple set of assignments, operators and submodules. 
    It already does basic type checking and assigns a type to every wire. 
    The Generating Structure of the code is not yet executed. 
    It is template-preserving
    */
    pub fn flatten(linker : &Linker, module : &Module) -> FlattenedModule {
        let mut instructions = FlatAlloc::new();
        let global_resolver = GlobalResolver::new(linker, module.link_info.file);
        let errors = ErrorCollector::new(module.link_info.file);
        let mut context = FlatteningContext{
            instructions: &mut instructions,
            errors: &errors,
            is_declared_in_this_module : true,
            linker : &global_resolver,
            local_variable_context : LocalVariableContext::new_initial(),
            type_list_for_naming : &linker.types,
            module,
        };

        let interface_ports = context.initialize_interface::<false>();
        
        context.flatten_code(&module.code);
        context.typecheck();
        context.generative_check();
        context.find_unused_variables(&interface_ports);

        FlattenedModule {
            resolved_globals : context.linker.extract_resolved_globals(),
            errors,
            instructions,
            interface_ports,
        }
    }
}
