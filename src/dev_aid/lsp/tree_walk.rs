
use std::ops::Deref;

use crate::{
    file_position::Span, flattening::{Declaration, DomainID, FlatID, Instruction, Interface, Module, ModuleInterfaceReference, PortID, SubModuleInstance, WireInstance, WireReference, WireReferenceRoot, WireSource, WrittenType}, linker::{FileData, FileUUID, Linker, ModuleUUID, NameElem}
};

#[derive(Clone, Copy, Debug)]
pub enum InModule<'linker> {
    NamedLocal(&'linker Declaration),
    NamedSubmodule(&'linker SubModuleInstance),
    Temporary(&'linker WireInstance),
}

#[derive(Clone, Copy, Debug)]
pub enum LocationInfo<'linker> {
    InModule(ModuleUUID, &'linker Module, FlatID, InModule<'linker>),
    Type(&'linker WrittenType),
    Global(NameElem),
    /// The contained module only refers to the module on which the port is defined
    /// No reference to the module in which the reference was found is provided
    Port(&'linker SubModuleInstance, &'linker Module, PortID),
    Interface(ModuleUUID, &'linker Module, DomainID, &'linker Interface)
}

/// Permits really efficient [RefersTo::refers_to_same_as] [LocationInfo] checking
#[derive(Clone, Copy, Debug)]
pub struct RefersTo {
    pub local : Option<(ModuleUUID, FlatID)>,
    pub global : Option<NameElem>,
    pub port : Option<(ModuleUUID, PortID)>,
    pub interface : Option<(ModuleUUID, DomainID)>
}

impl<'linker> From<LocationInfo<'linker>> for RefersTo {
    fn from(info : LocationInfo) -> Self {
        let mut result = RefersTo{
            local: None,
            global: None,
            port: None,
            interface : None,
        };
        match info {
            LocationInfo::InModule(md_id, md, flat_id, flat_obj) => {
                match flat_obj {
                    InModule::NamedLocal(_) => {
                        if let Some((port_id, _)) = md.ports.iter().find(|(_, port)| port.declaration_instruction == flat_id) {
                            result.port = Some((md_id, port_id));
                        }
                        result.local = Some((md_id, flat_id));
                    },
                    InModule::NamedSubmodule(_) => {
                        result.local = Some((md_id, flat_id));
                    },
                    InModule::Temporary(_) => {}
                }
            }
            LocationInfo::Type(_) => {}
            LocationInfo::Global(name_elem) => {
                result.global = Some(name_elem);
            }
            LocationInfo::Port(sm, md, p_id) => {
                result.local = Some((sm.module_uuid, md.ports[p_id].declaration_instruction));
                result.port = Some((sm.module_uuid, p_id))
            }
            LocationInfo::Interface(md_id, _md, i_id, _interface) => {
                result.interface = Some((md_id, i_id))
            }
        }
        result
    }
}

impl RefersTo {
    pub fn refers_to_same_as(&self, info : LocationInfo) -> bool {
        match info {
            LocationInfo::InModule(md_id, _, obj, _) => self.local == Some((md_id, obj)),
            LocationInfo::Type(_) => false,
            LocationInfo::Global(ne) => self.global == Some(ne),
            LocationInfo::Port(sm, _, p_id) => self.port == Some((sm.module_uuid, p_id)),
            LocationInfo::Interface(md_id, _, i_id, _) => self.interface == Some((md_id, i_id))
        }
    }
    pub fn is_global(&self) -> bool {
        self.global.is_some() | self.port.is_some() | self.interface.is_some()
    }
}

/// Walks the file, and provides all [LocationInfo]s. 
pub fn visit_all<'linker, Visitor : FnMut(Span, LocationInfo<'linker>)>(linker : &'linker Linker, file : &'linker FileData, visitor : Visitor) {
    let mut walker = TreeWalker {
        linker,
        visitor,
        should_prune: |_| false,
    };

    walker.walk_file(file);
}

/// Walks the file, and provides all [LocationInfo]s. 
pub fn visit_all_in_module<'linker, Visitor : FnMut(Span, LocationInfo<'linker>)>(linker : &'linker Linker, md_id : ModuleUUID, visitor : Visitor) {
    let mut walker = TreeWalker {
        linker,
        visitor,
        should_prune: |_| false,
    };

    walker.walk_module(md_id);
}

/// Walks the file, and finds the [LocationInfo] that is the most relevant
/// 
/// IE, the [LocationInfo] in the selection area that has the smallest span. 
pub fn get_selected_object<'linker>(linker : &'linker Linker, file : FileUUID, position : usize) -> Option<(Span, LocationInfo<'linker>)> {
    let file_data = &linker.files[file];
    
    let mut best_object : Option<LocationInfo<'linker>> = None;
    let mut best_span : Span = Span::MAX_POSSIBLE_SPAN;
    
    let mut walker = TreeWalker {
        linker,
        visitor : |span, info| {
            if span.size() <= best_span.size() {
                //assert!(span.size() < self.best_span.size());
                // May not be the case. Do prioritize later ones, as they tend to be nested
                best_span = span;
                best_object = Some(info);
            }
        },
        should_prune: |span| !span.contains_pos(position),
    };

    walker.walk_file(file_data);

    best_object.map(|v| (best_span, v))
}

struct TreeWalker<'linker, Visitor : FnMut(Span, LocationInfo<'linker>), Pruner : Fn(Span) -> bool> {
    linker : &'linker Linker,
    visitor : Visitor,
    should_prune : Pruner
}

impl<'linker, Visitor : FnMut(Span, LocationInfo<'linker>), Pruner : Fn(Span) -> bool> TreeWalker<'linker, Visitor, Pruner>  {
    fn visit(&mut self, span : Span, info : LocationInfo<'linker>) {
        if !(self.should_prune)(span) {
            (self.visitor)(span, info);
        }
    }

    fn walk_wire_ref(&mut self, md_id : ModuleUUID, md : &'linker Module, wire_ref : &'linker WireReference) {
        match &wire_ref.root {
            WireReferenceRoot::LocalDecl(decl_id, span) => {
                self.visit(*span, LocationInfo::InModule(md_id, md, *decl_id, InModule::NamedLocal(md.instructions[*decl_id].unwrap_wire_declaration())));
            }
            WireReferenceRoot::NamedConstant(cst, span) => {
                self.visit(*span, LocationInfo::Global(NameElem::Constant(*cst)))
            }
            WireReferenceRoot::SubModulePort(port) => {
                if let Some(span) = port.port_name_span {
                    let sm_instruction = md.instructions[port.submodule_decl].unwrap_submodule();
                    let submodule = &self.linker.modules[sm_instruction.module_uuid];
                    self.visit(span, LocationInfo::Port(sm_instruction, submodule, port.port));

                    // port_name_span being enabled means submodule_name_span is for sure
                    // And if port_name_span is invalid, then submodule_name_span points to a duplicate!
                    // So in effect, port_name_span validity is a proxy for non-duplicate-ness of submodule_name_span
                    self.visit(port.submodule_name_span.unwrap(), LocationInfo::InModule(md_id, md, port.submodule_decl, InModule::NamedSubmodule(md.instructions[port.submodule_decl].unwrap_submodule())));
                }
            }
        }
    }

    fn walk_type(&mut self, typ_expr : &'linker WrittenType) {
        let typ_expr_span = typ_expr.get_span();
        if !(self.should_prune)(typ_expr_span) {
            (self.visitor)(typ_expr_span, LocationInfo::Type(typ_expr));
            match typ_expr {
                WrittenType::Error(_) => {}
                WrittenType::Named(span, name_id) => {
                    self.visit(*span, LocationInfo::Global(NameElem::Type(*name_id)));
                }
                WrittenType::Array(_, arr_box) => {
                    let (arr_content_typ, _size_id, _br_span) = arr_box.deref();

                    self.walk_type(arr_content_typ)
                }
            }
        }
    }
    
    fn walk_interface_reference(&mut self, md_id : ModuleUUID, md : &'linker Module, iref : &ModuleInterfaceReference) {
        if let Some(submod_name_span) = iref.name_span {
            let submodule_instruction = iref.submodule_decl;
            let submodule = md.instructions[submodule_instruction].unwrap_submodule();
            self.visit(submod_name_span, LocationInfo::InModule(md_id, md, submodule_instruction, InModule::NamedSubmodule(submodule)));
            if iref.interface_span != submod_name_span {
                let submod_md = &self.linker.modules[submodule.module_uuid];
                let interface = &submod_md.interfaces[iref.submodule_interface];
                self.visit(iref.interface_span, LocationInfo::Interface(submodule.module_uuid, submod_md, iref.submodule_interface, interface));
            }
        }
    }

    fn walk_module(&mut self, md_id : ModuleUUID) {
        let md = &self.linker.modules[md_id];
        if !(self.should_prune)(md.link_info.span) {
            self.visit(md.link_info.name_span, LocationInfo::Global(NameElem::Module(md_id)));

            let mut interface_iter = md.interfaces.iter();
            // Skip main interface
            interface_iter.next();
            for (interface_id, interface) in interface_iter {
                self.visit(interface.name_span, LocationInfo::Interface(md_id, md, interface_id, interface));
            }

            for (id, inst) in &md.instructions {
                match inst {
                    Instruction::SubModule(sm) => {
                        self.visit(sm.module_name_span, LocationInfo::Global(NameElem::Module(sm.module_uuid)));
                        if let Some((_sm_name, sm_name_span)) = &sm.name {
                            self.visit(*sm_name_span, LocationInfo::InModule(md_id, md, id, InModule::NamedSubmodule(sm)));
                        }
                    }
                    Instruction::Declaration(decl) => {
                        self.walk_type(&decl.typ_expr);
                        if decl.declaration_itself_is_not_written_to {
                            self.visit(decl.name_span, LocationInfo::InModule(md_id, md, id, InModule::NamedLocal(decl)));
                        }
                    }
                    Instruction::Wire(wire) => {
                        if let WireSource::WireRef(wire_ref) = &wire.source {
                            self.walk_wire_ref(md_id, md, wire_ref);
                        } else {
                            self.visit(wire.span, LocationInfo::InModule(md_id, md, id, InModule::Temporary(wire)));
                        };
                    }
                    Instruction::Write(write) => {
                        self.walk_wire_ref(md_id, md, &write.to);
                    }
                    Instruction::FuncCall(fc) => {
                        self.walk_interface_reference(md_id, md, &fc.interface_reference);
                    }
                    Instruction::IfStatement(_) | Instruction::ForStatement(_) => {}
                };
            }
        }
    }

    fn walk_file(&mut self, file : &'linker FileData) {
        for global in &file.associated_values {
            match *global {
                NameElem::Module(md_id) => {
                    self.walk_module(md_id);
                }
                NameElem::Type(_) => {
                    todo!()
                }
                NameElem::Constant(_) => {
                    todo!()
                }
            }
        }
    }
}
