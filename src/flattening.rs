use std::{ops::Deref, iter::zip};

use crate::{
    ast::{Span, Module, Expression, SpanExpression, LocalOrGlobal, Operator, AssignableExpression, SpanAssignableExpression, Statement, CodeBlock, IdentifierType, TypeExpression, DeclIDMarker, DeclID, SpanTypeExpression, InterfacePorts},
    linker::{Linker, FileUUID, GlobalResolver, ResolvedGlobals, NamedConstant, ConstantUUID, ModuleUUID, NameElem, NamedType, TypeUUIDMarker},
    errors::{ErrorCollector, error_info, ErrorInfo}, arena_alloc::{UUID, UUIDMarker, FlatAlloc, UUIDRange, ArenaAllocator}, typing::{Type, typecheck_unary_operator, get_binary_operator_types, typecheck, typecheck_is_array_indexer, BOOL_TYPE, INT_TYPE}, value::Value
};

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub struct FlatIDMarker;
impl UUIDMarker for FlatIDMarker {const DISPLAY_NAME : &'static str = "obj_";}
pub type FlatID = UUID<FlatIDMarker>;

pub type FlatIDRange = UUIDRange<FlatIDMarker>;

#[derive(Debug)]
pub enum ConnectionWritePathElement {
    ArrayIdx{idx : FlatID, idx_span : Span},
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
    pub path : Vec<ConnectionWritePathElement>,
    pub span : Span,
    pub is_remote_declaration : bool
}

#[derive(Debug)]
pub struct Connection {
    pub num_regs : i64,
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
    pub is_remote_declaration : bool,
    pub source : WireSource
}

#[derive(Debug)]
pub struct WireDeclaration {
    pub typ : Type,
    pub typ_span : Span,
    pub is_remote_declaration : bool,
    pub name_token : usize,
    pub name : Box<str>,
    pub read_only : bool,
    pub identifier_type : IdentifierType,
    pub latency_specifier : Option<FlatID>
}
impl WireDeclaration {
    pub fn get_full_decl_span(&self) -> Span {
        let name_pos = self.name_token;
        Span(self.typ_span.0, name_pos)
    }
}

#[derive(Debug)]
pub struct SubModuleInstance {
    pub module_uuid : ModuleUUID,
    pub name : Box<str>,
    pub typ_span : Span,
    pub is_remote_declaration : bool,
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
pub enum Instantiation {
    SubModule(SubModuleInstance),
    WireDeclaration(WireDeclaration),
    Wire(WireInstance),
    Connection(Connection),
    IfStatement(IfStatement),
    ForStatement(ForStatement)
}

impl Instantiation {
    #[track_caller]
    pub fn extract_wire(&self) -> &WireInstance {
        let Self::Wire(w) = self else {panic!("extract_wire on not a wire! Found {self:?}")};
        w
    }
    #[track_caller]
    pub fn extract_wire_declaration(&self) -> &WireDeclaration {
        let Self::WireDeclaration(w) = self else {panic!("extract_wire on not a WireDeclaration! Found {self:?}")};
        w
    }
    #[track_caller]
    pub fn extract_submodule(&self) -> &SubModuleInstance {
        let Self::SubModule(sm) = self else {panic!("extract_wire on not a SubModule! Found {self:?}")};
        sm
    }

    pub fn for_each_embedded_type<F : FnMut(&Type, Span)>(&self, f : &mut F) {
        match self {
            Instantiation::SubModule(_) => {}
            Instantiation::Connection(_) => {}
            Instantiation::IfStatement(_) => {}
            Instantiation::ForStatement(_) => {}
            Instantiation::WireDeclaration(decl) => {
                f(&decl.typ, decl.typ_span);
            }
            Instantiation::Wire(w) => {
                f(&w.typ, w.span);
            }
        }
    }

    pub fn get_location(&self) -> Option<(Span, bool)> {
        match self {
            Instantiation::SubModule(sm) => Some((sm.typ_span, sm.is_remote_declaration)),
            Instantiation::WireDeclaration(decl) => Some((decl.typ_span, decl.is_remote_declaration)),
            Instantiation::Wire(w) => Some((w.span, w.is_remote_declaration)),
            Instantiation::Connection(conn) => Some((conn.to.span, conn.to.is_remote_declaration)),
            Instantiation::IfStatement(_) | Instantiation::ForStatement(_) => None
        }
    }
}

struct FlatteningContext<'inst, 'l, 'm> {
    decl_to_flat_map : FlatAlloc<Option<FlatID>, DeclIDMarker>,
    instantiations : &'inst mut FlatAlloc<Instantiation, FlatIDMarker>,
    errors : ErrorCollector,
    is_remote_declaration : bool,

    linker : GlobalResolver<'l>,
    pub type_list_for_naming : &'l ArenaAllocator<NamedType, TypeUUIDMarker>,
    module : &'m Module,
}

impl<'inst, 'l, 'm> FlatteningContext<'inst, 'l, 'm> {
    fn map_to_type(&mut self, type_expr : &SpanTypeExpression) -> Type {
        match &type_expr.0 {
            TypeExpression::Named => {
                if let Some(typ_id) = &self.linker.resolve_type(type_expr.1, &self.errors) {
                    Type::Named{id : *typ_id, span : Some(type_expr.1)}
                } else {
                    Type::Error
                }
            }
            TypeExpression::Array(b) => {
                let (array_type_expr, array_size_expr) = b.deref();
                let array_element_type = self.map_to_type(&array_type_expr);
                if let Some(array_size_wire_id) = self.flatten_expr(array_size_expr) {
                    Type::Array(Box::new((array_element_type, array_size_wire_id)))
                } else {
                    Type::Error
                }
            }
        }
    }
    fn flatten_declaration<const ALLOW_MODULES : bool>(&mut self, decl_id : DeclID, read_only : bool) -> FlatID {
        let decl = &self.module.declarations[decl_id];

        let typ : Type = if let TypeExpression::Named = &decl.typ.0 {
            match self.linker.resolve_global(decl.typ.1, &self.errors) {
                Some(NameElem::Module(id)) if ALLOW_MODULES => {
                    let md = &self.linker.get_module(id);
                    return self.alloc_module_interface(decl.name.clone(), md, id, decl.typ.1)
                }
                Some(NameElem::Type(id)) => {
                    Type::Named{id, span : Some(decl.typ.1)}
                }
                Some(global_module_or_type) => {
                    let accepted = if ALLOW_MODULES {"Type or Module"} else {"Type"};
                    self.linker.make_bad_error_location_error(global_module_or_type, accepted, decl.typ.1, &self.errors);
                    Type::Error
                }
                None => Type::Error
            }
        } else {
            self.map_to_type(&decl.typ)
        };

        let typ_span = decl.typ.1;

        let latency_specifier = if let Some(lat_expr) = &decl.latency_expr {
            if let Some(latency_spec) = self.flatten_expr(lat_expr) {
                self.must_be_compiletime(self.instantiations[latency_spec].extract_wire(), "Latency specifier");
                Some(latency_spec)
            } else {
                None
            }
        } else {
            None
        };

        let inst_id = self.instantiations.alloc(Instantiation::WireDeclaration(WireDeclaration{
            typ,
            typ_span,
            is_remote_declaration : self.is_remote_declaration,
            read_only,
            identifier_type : decl.identifier_type,
            name : decl.name.clone(),
            name_token : decl.name_token,
            latency_specifier
        }));

        self.decl_to_flat_map[decl_id] = Some(inst_id);
        inst_id
    }
    fn initialize_interface<const IS_SUBMODULE : bool>(&mut self) -> InterfacePorts<FlatID> {
        self.module.ports.map(&mut |id, is_input|{
            let read_only = is_input ^ IS_SUBMODULE;

            self.flatten_declaration::<false>(id, read_only)
        })
    }
    fn alloc_module_interface(&mut self, name : Box<str>, module : &Module, module_uuid : ModuleUUID, typ_span : Span) -> FlatID {
        let mut nested_context = FlatteningContext {
            decl_to_flat_map: module.declarations.iter().map(|_| None).collect(),
            instantiations: self.instantiations,
            errors: ErrorCollector::new(module.link_info.file), // Temporary ErrorCollector, unused
            is_remote_declaration: true,
            linker: self.linker.new_sublinker(module.link_info.file),
            type_list_for_naming: self.type_list_for_naming,
            module,
        };
        
        let interface_ports = nested_context.initialize_interface::<true>();
        
        self.linker.reabsorb_sublinker(nested_context.linker);

        self.instantiations.alloc(Instantiation::SubModule(SubModuleInstance{
            name,
            module_uuid,
            is_remote_declaration : self.is_remote_declaration,
            typ_span,
            interface_ports
        }))
    }
    // Returns the module, full interface, and the output range for the function call syntax
    fn desugar_func_call(&mut self, func_and_args : &[SpanExpression], closing_bracket_pos : usize) -> Option<(&Module, InterfacePorts<FlatID>)> {
        let (name_expr, name_expr_span) = &func_and_args[0]; // Function name is always there
        let func_instantiation_id = match name_expr {
            Expression::Named(LocalOrGlobal::Local(l)) => {
                self.decl_to_flat_map[*l].unwrap()
            }
            Expression::Named(LocalOrGlobal::Global(ref_span)) => {
                let module_id = self.linker.resolve_module(*ref_span, &self.errors)?;
                let md = &self.linker.get_module(module_id);
                self.alloc_module_interface(md.link_info.name.clone(), md, module_id, *name_expr_span)
            }
            _other => {
                self.errors.error_basic(*name_expr_span, "Function call name cannot be an expression");
                return None;
            }
        };
        let func_instantiation = &self.instantiations[func_instantiation_id].extract_submodule();
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
                let excess_args_span = Span(args[expected_arg_count].1.0, closing_bracket_pos - 1);
                self.errors.error_with_info(excess_args_span, format!("Excess argument. Function takes {expected_arg_count} args, but {arg_count} were passed."), module_info);
                // Shorten args to still get proper type checking for smaller arg array
                args = &args[..expected_arg_count];
            } else {
                // Too few args, mention missing argument names
                self.errors.error_with_info(Span::from(closing_bracket_pos), format!("Too few arguments. Function takes {expected_arg_count} args, but {arg_count} were passed."), module_info);
            }
        }

        for (field, arg_expr) in zip(inputs, args) {
            if let Some(arg_read_side) = self.flatten_expr(arg_expr) {
                /*if self.typecheck(arg_read_side, &md.interface.interface_wires[field].typ, "submodule output") == None {
                    continue;
                }*/
                let func_input_port = &submodule_local_wires.ports[field];
                self.instantiations.alloc(Instantiation::Connection(Connection{num_regs: 0, from: arg_read_side, to: ConnectionWrite{root : *func_input_port, path : Vec::new(), span : *name_expr_span, is_remote_declaration : self.is_remote_declaration}}));
            }
        }

        Some((md, submodule_local_wires))
    }
    fn flatten_expr(&mut self, (expr, expr_span) : &SpanExpression) -> Option<FlatID> {
        let source = match expr {
            Expression::Named(LocalOrGlobal::Local(l)) => {
                let from_wire = self.decl_to_flat_map[*l].unwrap();
                WireSource::WireRead(from_wire)
            }
            Expression::Named(LocalOrGlobal::Global(ref_span)) => {
                let cst = self.linker.resolve_constant(*ref_span, &self.errors)?;
                WireSource::NamedConstant(cst)
            }
            Expression::Constant(cst) => {
                WireSource::Constant(cst.clone())
            }
            Expression::UnaryOp(op_box) => {
                let (op, _op_pos, operate_on) = op_box.deref();
                let right = self.flatten_expr(operate_on)?;
                WireSource::UnaryOp{op : *op, right}
            }
            Expression::BinOp(binop_box) => {
                let (left_expr, op, _op_pos, right_expr) = binop_box.deref();
                let left = self.flatten_expr(left_expr)?;
                let right = self.flatten_expr(right_expr)?;
                WireSource::BinaryOp{op : *op, left, right}
            }
            Expression::Array(arr_box) => {
                let (left, right, _bracket_span) = arr_box.deref();
                let arr = self.flatten_expr(left)?;
                let arr_idx = self.flatten_expr(right)?;
                WireSource::ArrayAccess{arr, arr_idx}
            }
            Expression::FuncCall(func_and_args) => {
                let (md, interface_wires) = self.desugar_func_call(func_and_args, expr_span.1)?;

                let output_range = interface_wires.func_call_syntax_outputs();

                if output_range.len() != 1 {
                    let info = error_info(md.link_info.span, md.link_info.file, "Module Defined here");
                    self.errors.error_with_info(*expr_span, "A function called in this context may only return one result. Split this function call into a separate line instead.", vec![info]);
                    return None;
                }

                return Some(interface_wires.ports[output_range.start])
            }
        };

        let wire_instance = WireInstance{typ : Type::Unknown, is_compiletime : IS_GEN_UNINIT, span : *expr_span, source, is_remote_declaration : self.is_remote_declaration,};
        Some(self.instantiations.alloc(Instantiation::Wire(wire_instance)))
    }
    fn flatten_assignable_expr(&mut self, (expr, span) : &SpanAssignableExpression) -> Option<ConnectionWrite> {
        Some(match expr {
            AssignableExpression::Named{local_idx} => {
                let root = self.decl_to_flat_map[*local_idx].unwrap();
                let decl = self.instantiations[root].extract_wire_declaration();

                if decl.read_only {
                    let decl_info = error_info(self.module.declarations[*local_idx].span, self.errors.file, "Declared here");
                    self.errors.error_with_info(*span, "Cannot Assign to Read-Only value", vec![decl_info]);
                    return None
                }
                ConnectionWrite{root, path : Vec::new(), span : *span, is_remote_declaration : self.is_remote_declaration,}
            }
            AssignableExpression::ArrayIndex(arr_box) => {
                let (arr, idx_expr, _bracket_span) = arr_box.deref();
                let flattened_arr_expr_opt = self.flatten_assignable_expr(arr);
                
                let idx = self.flatten_expr(idx_expr)?;

                let mut flattened_arr_expr = flattened_arr_expr_opt?; // only unpack the subexpr after flattening the idx, so we catch all errors

                flattened_arr_expr.path.push(ConnectionWritePathElement::ArrayIdx{idx, idx_span : idx_expr.1});

                flattened_arr_expr
            }
        })
    }
    fn flatten_code(&mut self, code : &CodeBlock) {
        for (stmt, stmt_span) in &code.statements {
            match stmt {
                Statement::Declaration(decl_id) => {
                    let _wire_id = self.flatten_declaration::<true>(*decl_id, false);
                }
                Statement::Assign{to, expr : (Expression::FuncCall(func_and_args), func_span), eq_sign_position} => {
                    let Some((md, interface)) = self.desugar_func_call(&func_and_args, func_span.1) else {continue};
                    let output_range = interface.func_call_syntax_outputs();
                    let outputs = &interface.ports[output_range];

                    let func_name_span = func_and_args[0].1;
                    let num_func_outputs = outputs.len();
                    let num_targets = to.len();
                    if num_targets != num_func_outputs {
                        let info = vec![error_info(md.link_info.span, md.link_info.file, "Module Defined here")];
                        if num_targets > num_func_outputs {
                            let excess_results_span = Span(to[num_func_outputs].expr.1.0, to.last().unwrap().expr.1.1);
                            self.errors.error_with_info(excess_results_span, format!("Excess output targets. Function returns {num_func_outputs} results, but {num_targets} targets were given."), info);
                        } else {
                            let too_few_targets_pos = if let Some(eq) = eq_sign_position {Span::from(*eq)} else {func_name_span};
                            self.errors.error_with_info(too_few_targets_pos, format!("Too few output targets. Function returns {num_func_outputs} results, but {num_targets} targets were given."), info);
                        }
                    }

                    for (field, to_i) in zip(outputs, to) {
                        let Some(write_side) = self.flatten_assignable_expr(&to_i.expr) else {continue};

                        // temporary
                        let module_port_wire_decl = self.instantiations[*field].extract_wire_declaration();
                        let module_port_proxy = self.instantiations.alloc(Instantiation::Wire(WireInstance{typ : module_port_wire_decl.typ.clone(), is_compiletime : IS_GEN_UNINIT, span : *func_span, is_remote_declaration : self.is_remote_declaration, source : WireSource::WireRead(*field)}));
                        self.instantiations.alloc(Instantiation::Connection(Connection{num_regs : to_i.num_regs, from: module_port_proxy, to: write_side}));
                    }
                },
                Statement::Assign{to, expr : non_func_expr, eq_sign_position : _} => {
                    if to.len() == 1 {
                        let Some(read_side) = self.flatten_expr(non_func_expr) else {continue};
                        let t = &to[0];
                        let Some(write_side) = self.flatten_assignable_expr(&t.expr) else {continue};
                        self.instantiations.alloc(Instantiation::Connection(Connection{num_regs : t.num_regs, from: read_side, to: write_side}));
                    } else {
                        self.errors.error_basic(*stmt_span, format!("Non-function assignments must only output exactly 1 instead of {}", to.len()));
                    }
                },
                Statement::Block(inner_code) => {
                    self.flatten_code(inner_code);
                },
                Statement::If{condition : condition_expr, then, els} => {
                    let Some(condition) = self.flatten_expr(condition_expr) else {continue};

                    let if_id = self.instantiations.alloc(Instantiation::IfStatement(IfStatement{condition, then_start : UUID::PLACEHOLDER, then_end_else_start : UUID::PLACEHOLDER, else_end : UUID::PLACEHOLDER}));
                    let then_start = self.instantiations.get_next_alloc_id();

                    self.flatten_code(then);
                    let then_end_else_start = self.instantiations.get_next_alloc_id();
                    if let Some(e) = els {
                        self.flatten_code(e);
                    }
                    let else_end = self.instantiations.get_next_alloc_id();

                    let Instantiation::IfStatement(if_stmt) = &mut self.instantiations[if_id] else {unreachable!()};
                    if_stmt.then_start = then_start;
                    if_stmt.then_end_else_start = then_end_else_start;
                    if_stmt.else_end = else_end;
                }
                Statement::For{var : decl_id, range, code} => {
                    let loop_var_decl = self.flatten_declaration::<false>(*decl_id, true);

                    let start = self.flatten_expr(&range.from);
                    let end = self.flatten_expr(&range.to);
                    
                    let for_id = if let (Some(start), Some(end)) = (start, end) {
                        Some(self.instantiations.alloc(Instantiation::ForStatement(ForStatement{loop_var_decl, start, end, loop_body: UUIDRange(UUID::PLACEHOLDER, UUID::PLACEHOLDER)})))
                    } else {
                        None
                    };

                    let code_start = self.instantiations.get_next_alloc_id();

                    self.flatten_code(code);
                    
                    let code_end = self.instantiations.get_next_alloc_id();

                    if let Some(for_id) = for_id {
                        let Instantiation::ForStatement(for_stmt) = &mut self.instantiations[for_id] else {unreachable!()};

                        for_stmt.loop_body = UUIDRange(code_start, code_end);
                    }
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

    fn typecheck(&mut self) {
        let look_at_queue : Vec<FlatID> = self.instantiations.iter().map(|(id,_)| id).collect();

        for elem_id in look_at_queue {
            match &self.instantiations[elem_id] {
                Instantiation::SubModule(_) => {}
                Instantiation::WireDeclaration(decl) => {
                    if let Some(latency_spec) = decl.latency_specifier {
                        let latency_spec_wire = &self.instantiations[latency_spec].extract_wire();
                        self.typecheck_wire_is_of_type(latency_spec_wire, &INT_TYPE, "latency specifier");
                    }

                    decl.typ.for_each_generative_input(&mut |param_id| {
                        self.typecheck_wire_is_of_type(self.instantiations[param_id].extract_wire(), &INT_TYPE, "Array size");
                    });
                }
                Instantiation::IfStatement(stm) => {
                    let wire = &self.instantiations[stm.condition].extract_wire();
                    self.typecheck_wire_is_of_type(wire, &BOOL_TYPE, "if statement condition")
                }
                Instantiation::ForStatement(stm) => {
                    let loop_var = &self.instantiations[stm.loop_var_decl].extract_wire_declaration();
                    let start = &self.instantiations[stm.start].extract_wire();
                    let end = &self.instantiations[stm.end].extract_wire();
                    self.typecheck_wire_is_of_type(start, &loop_var.typ, "for loop");
                    self.typecheck_wire_is_of_type(end, &loop_var.typ, "for loop");
                }
                Instantiation::Wire(w) => {
                    let result_typ = match &w.source {
                        &WireSource::WireRead(from_wire) => {
                            self.instantiations[from_wire].extract_wire_declaration().typ.clone()
                        }
                        &WireSource::UnaryOp{op, right} => {
                            let right_wire = self.instantiations[right].extract_wire();
                            typecheck_unary_operator(op, &right_wire.typ, right_wire.span, self.type_list_for_naming, &self.errors)
                        }
                        &WireSource::BinaryOp{op, left, right} => {
                            let left_wire = self.instantiations[left].extract_wire();
                            let right_wire = self.instantiations[right].extract_wire();
                            let ((input_left_type, input_right_type), output_type) = get_binary_operator_types(op);
                            self.typecheck_wire_is_of_type(left_wire, &input_left_type, &format!("{op} left"));
                            self.typecheck_wire_is_of_type(right_wire, &input_right_type, &format!("{op} right"));
                            output_type
                        }
                        &WireSource::ArrayAccess{arr, arr_idx} => {
                            let arr_wire = self.instantiations[arr].extract_wire();
                            let arr_idx_wire = self.instantiations[arr_idx].extract_wire();
                
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
                    let Instantiation::Wire(w) = &mut self.instantiations[elem_id] else {unreachable!()};
                    w.typ = result_typ;
                }
                Instantiation::Connection(conn) => {
                    // Typecheck digging down into write side
                    let conn_root = self.instantiations[conn.to.root].extract_wire_declaration();
                    let mut write_to_type = Some(&conn_root.typ);
                    for p in &conn.to.path {
                        match p {
                            &ConnectionWritePathElement::ArrayIdx{idx, idx_span} => {
                                let idx_wire = self.instantiations[idx].extract_wire();
                                self.typecheck_wire_is_of_type(idx_wire, &INT_TYPE, "array index");
                                if let Some(wr) = write_to_type {
                                    write_to_type = typecheck_is_array_indexer(wr, idx_span, self.type_list_for_naming, &self.errors);
                                }
                            }
                        }
                    }

                    // Typecheck the value with target type
                    let from_wire = self.instantiations[conn.from].extract_wire();
                    if let Some(target_type) = write_to_type {
                        self.typecheck_wire_is_of_type(from_wire, &target_type, "connection");
                    }
                }
            }
        }

        // Post type application. Flag any remaining Type::Unknown
        for (_id, inst) in self.instantiations.iter() {
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

        let mut declaration_depths : FlatAlloc<Option<usize>, FlatIDMarker> = self.instantiations.iter().map(|_| None).collect();

        for inst_id in self.instantiations.id_range() {
            while let Some((end_id, span)) = runtime_if_stack.pop() {
                if end_id != inst_id {
                    runtime_if_stack.push((end_id, span));
                    break;
                }
            }
            match &self.instantiations[inst_id] {
                Instantiation::SubModule(_) => {}
                Instantiation::WireDeclaration(decl) => {
                    if decl.identifier_type == IdentifierType::Generative {
                        assert!(declaration_depths[inst_id].is_none());
                        declaration_depths[inst_id] = Some(runtime_if_stack.len())
                    }

                    decl.typ.for_each_generative_input(&mut |param_id| {
                        self.must_be_compiletime(self.instantiations[param_id].extract_wire(), "Array size");
                    });
                }

                Instantiation::Wire(wire) => {
                    let mut is_generative = true;
                    if let WireSource::WireRead(from) = &wire.source {
                        let decl = self.instantiations[*from].extract_wire_declaration();
                        if decl.identifier_type != IdentifierType::Generative {
                            is_generative = false;
                        }
                    } else {
                        wire.source.for_each_input_wire(&mut |source_id| {
                            let source_wire = self.instantiations[source_id].extract_wire();
                            if !source_wire.is_compiletime {
                                is_generative = false;
                            }
                        });
                    }
                    let Instantiation::Wire(wire) = &mut self.instantiations[inst_id] else {unreachable!()};
                    wire.is_compiletime = is_generative;
                }
                Instantiation::Connection(conn) => {
                    let conn_root_decl = self.instantiations[conn.to.root].extract_wire_declaration();

                    if conn_root_decl.identifier_type == IdentifierType::Generative {
                        let from_wire = self.instantiations[conn.from].extract_wire();
                        // Check that whatever's written to this declaration is also generative
                        self.must_be_compiletime_with_info(from_wire, "Assignments to generative variables", || vec![error_info(conn_root_decl.get_full_decl_span(), self.errors.file, "Declared here")]);

                        // Check that this declaration isn't used in a non-compiletime if
                        let declared_at_depth = declaration_depths[conn.to.root].unwrap();
    
                        if runtime_if_stack.len() > declared_at_depth {
                            let mut infos = Vec::new();
                            infos.push(error_info(conn_root_decl.get_full_decl_span(), self.errors.file, "Declared here"));
                            for (_, if_cond_span) in &runtime_if_stack[declared_at_depth..] {
                                infos.push(error_info(*if_cond_span, self.errors.file, "Runtime Condition here"));
                            }
                            self.errors.error_with_info(conn.to.span, "Cannot write to generative variables in runtime conditional block", infos);
                        }
                    }
                }
                Instantiation::IfStatement(if_stmt) => {
                    let condition_wire = self.instantiations[if_stmt.condition].extract_wire();
                    if !condition_wire.is_compiletime {
                        runtime_if_stack.push((if_stmt.else_end, condition_wire.span));
                    }
                }
                Instantiation::ForStatement(_) => {}
            }
        }
    }

    /* 
        ==== Additional Warnings ====
    */
    fn find_unused_variables(&self, interface : &InterfacePorts<FlatID>) {
        // Setup Wire Fanouts List for faster processing
        let mut gathered_connection_fanin : FlatAlloc<Vec<FlatID>, FlatIDMarker> = self.instantiations.iter().map(|_| Vec::new()).collect();

        for (inst_id, inst) in self.instantiations.iter() {
            match inst {
                Instantiation::Connection(conn) => {
                    gathered_connection_fanin[conn.to.root].push(conn.from);
                }
                Instantiation::SubModule(sm) => {
                    for w in sm.interface_ports.outputs() {
                        gathered_connection_fanin[*w].push(inst_id);
                    }
                }
                Instantiation::WireDeclaration(_) => {} // Handle these outside
                Instantiation::Wire(_) => {}
                Instantiation::IfStatement(stm) => {
                    for id in UUIDRange(stm.then_start, stm.else_end) {
                        if let Instantiation::Connection(conn) = &self.instantiations[id] {
                            gathered_connection_fanin[conn.to.root].push(stm.condition);
                        }
                    }
                }
                Instantiation::ForStatement(stm) => {
                    gathered_connection_fanin[stm.loop_var_decl].push(stm.start);
                    gathered_connection_fanin[stm.loop_var_decl].push(stm.end);
                }
            }
        }

        let mut is_instance_used_map : FlatAlloc<bool, FlatIDMarker> = self.instantiations.iter().map(|_| false).collect();

        let mut wire_to_explore_queue : Vec<FlatID> = Vec::new();

        for port in interface.outputs() {
            is_instance_used_map[*port] = true;
            wire_to_explore_queue.push(*port);
        }

        while let Some(item) = wire_to_explore_queue.pop() {
            let mut mark_not_unused = |from| {
                if !is_instance_used_map[from] {
                    is_instance_used_map[from] = true;
                    wire_to_explore_queue.push(from);
                }
            };
            match &self.instantiations[item] {
                Instantiation::WireDeclaration(decl) => {
                    decl.typ.for_each_generative_input(&mut mark_not_unused);
                }
                Instantiation::Wire(wire) => {
                    wire.source.for_each_input_wire(&mut mark_not_unused);
                }
                Instantiation::SubModule(submodule) => {
                    for port in submodule.interface_ports.inputs() {
                        mark_not_unused(*port);
                    }
                }
                Instantiation::Connection(_) | Instantiation::IfStatement(_) | Instantiation::ForStatement(_) => {unreachable!()}
            }
            for from in &gathered_connection_fanin[item] {
                mark_not_unused(*from);
            }
        }

        // Now produce warnings from the unused list
        for (id, inst) in self.instantiations.iter() {
            if !is_instance_used_map[id] {
                if let Instantiation::WireDeclaration(decl) = inst {
                    if !decl.is_remote_declaration {
                        self.errors.warn_basic(Span::from(decl.name_token), "Unused Variable: This variable does not affect the output ports of this module");
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
    pub instantiations : FlatAlloc<Instantiation, FlatIDMarker>,
    pub errors : ErrorCollector,
    pub interface_ports : InterfacePorts<FlatID>,
    pub resolved_globals : ResolvedGlobals
}

impl FlattenedModule {
    pub fn empty(file : FileUUID) -> FlattenedModule {
        FlattenedModule {
            instantiations : FlatAlloc::new(),
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
    pub fn initialize(linker : &Linker, module : &Module) -> FlattenedModule {
        let mut instantiations = FlatAlloc::new();
        let mut context = FlatteningContext{
            decl_to_flat_map: module.declarations.iter().map(|_| None).collect(),
            instantiations: &mut instantiations,
            errors: ErrorCollector::new(module.link_info.file),
            is_remote_declaration : false,
            linker : GlobalResolver::new(linker, module.link_info.file),
            type_list_for_naming : &linker.types,
            module,
        };

        let interface_ports = context.initialize_interface::<false>();
        
        context.flatten_code(&module.code);
        context.typecheck();
        context.generative_check();
        context.find_unused_variables(&interface_ports);

        FlattenedModule {
            errors : context.errors,
            resolved_globals : context.linker.extract_resolved_globals(),
            instantiations : instantiations,
            interface_ports,
        }
    }
}
