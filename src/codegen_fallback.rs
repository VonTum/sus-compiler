use std::ops::Deref;

use crate::{concrete_type::ConcreteType, flattening::{DeclarationPortInfo, Instruction, Module}, instantiation::{InstantiatedModule, RealWire, RealWireDataSource, RealWirePathElem, WireID, CALCULATE_LATENCY_LATER}, linker::{get_builtin_type, TypeUUID}, value::Value};

fn get_type_name_size(id : TypeUUID) -> u64 {
    if id == get_builtin_type("int") {
        32 // TODO concrete int sizes
    } else if id == get_builtin_type("bool") {
        1
    } else {
        println!("TODO Named Structs Size");
        1 // todo!() // Named structs are not implemented yet
    }
}

fn typ_to_verilog_array(typ : &ConcreteType) -> String {
    match typ {
        ConcreteType::Named(id) => {
            let sz = get_type_name_size(*id);
            if sz == 1 {
                String::new()
            } else {
                format!("[{}:0]", sz - 1)
            }
        }
        ConcreteType::Array(arr) => {
            let (sub_typ, size) = arr.deref();
            let sz = size.unwrap_value().unwrap_integer();
            typ_to_verilog_array(sub_typ) + &format!("[{}:0]", sz - 1)
        }
        &ConcreteType::Value(_) | ConcreteType::Unknown | ConcreteType::Error => unreachable!()
    }
}

struct CodeGenerationContext<'g, 'out, Stream : std::fmt::Write> {
    md : &'g Module,
    instance : &'g InstantiatedModule,
    program_text : &'out mut Stream,

    use_latency : bool
}

fn wire_name_with_latency(wire : &RealWire, absolute_latency : i64, use_latency : bool) -> String {
    assert!(wire.absolute_latency <= absolute_latency);
    assert!(wire.needed_until >= absolute_latency);
    
    if use_latency && (wire.absolute_latency != absolute_latency) {
        format!("{}_D{}", wire.name, absolute_latency)
    } else {
        wire.name.to_string()
    }
}

fn wire_name_self_latency(wire : &RealWire, use_latency : bool) -> String {
    wire_name_with_latency(wire, wire.absolute_latency, use_latency)
}

impl<'g, 'out, Stream : std::fmt::Write> CodeGenerationContext<'g, 'out, Stream> {
    /// This is for making the resulting Verilog a little nicer to read
    fn can_inline(&self, wire : &RealWire) -> bool {
        match &wire.source {
            RealWireDataSource::Constant { value } => {
                match value {
                    Value::Bool(_) | Value::Integer(_) => true,
                    _other => false
                }
            }
            _other => false,
        }
    }
    
    fn operation_to_string(&self, wire : &RealWire) -> String {
        match &wire.source {
            RealWireDataSource::Constant { value } => value.to_string(),
            _other => unreachable!()
        }
    }

    fn wire_name(&self, wire_id : WireID, requested_latency : i64) -> String {
        let wire = &self.instance.wires[wire_id];
        if self.can_inline(wire) {
            self.operation_to_string(wire)
        } else {
            wire_name_with_latency(wire, requested_latency, self.use_latency)
        }
    }

    fn wire_ref_path_to_string(&self, path : &[RealWirePathElem], absolute_latency : i64) -> String {
        let mut result = String::new();
        for path_elem in path {
            result.push_str(&match path_elem {
                RealWirePathElem::ArrayAccess{span:_, idx_wire} => {
                    let idx_wire_name = self.wire_name(*idx_wire, absolute_latency);
                    format!("[{idx_wire_name}]")
                }
            });
        }
        result
    }
    
    fn add_latency_registers(&mut self, w : &RealWire) -> Result<(), std::fmt::Error> {
        if self.use_latency {
            let type_str = typ_to_verilog_array(&w.typ);

            // Can do 0 iterations, when w.needed_until == w.absolute_latency. Meaning it's only needed this cycle
            assert!(w.absolute_latency != CALCULATE_LATENCY_LATER);
            assert!(w.needed_until != CALCULATE_LATENCY_LATER);
            for i in w.absolute_latency..w.needed_until {
                let from = wire_name_with_latency(w, i, self.use_latency);
                let to = wire_name_with_latency(w, i+1, self.use_latency);

                writeln!(self.program_text, "/*latency*/ reg{type_str} {to}; always @(posedge clk) begin {to} <= {from}; end")?;
            }
        }
        Ok(())
    }

    fn write_verilog_code(&mut self) -> Result<(), std::fmt::Error> {
        // First output the interface of the module
        writeln!(self.program_text, "module {}(", mangle(&self.instance.name))?;
        writeln!(self.program_text, "\tinput clk,")?;
        for (_id, port) in self.instance.interface_ports.iter_valids() {
            let port_wire = &self.instance.wires[port.wire];
            let input_or_output = if port.is_input {"input"} else {"output /*mux_wire*/ reg"};
            let wire_typ = typ_to_verilog_array(&port_wire.typ);
            let wire_name = wire_name_self_latency(port_wire, self.use_latency);
            writeln!(self.program_text, "\t{input_or_output}{wire_typ} {wire_name},")?;
        }
        writeln!(self.program_text, ");\n")?;
        for (_id, port) in self.instance.interface_ports.iter_valids() {
            let port_wire = &self.instance.wires[port.wire];
            self.add_latency_registers(port_wire)?;
        }

        // Then output all declarations, and the wires we can already assign
        for (_id, w) in &self.instance.wires {
            // For better readability of output Verilog
            if self.can_inline(w) {continue;}

            if let Instruction::Declaration(wire_decl) = &self.md.instructions[w.original_instruction] {
                // Don't print named inputs and outputs, already did that in interface
                if let DeclarationPortInfo::RegularPort { is_input: _, port_id:_ } = wire_decl.is_port {
                    continue;
                }
            }
            let wire_or_reg = if let RealWireDataSource::Multiplexer{is_state, sources: _} = &w.source {
                if is_state.is_some() {
                    "reg"
                } else {
                    "/*mux_wire*/ reg"
                }
            } else {"wire"};

            let wire_name = wire_name_self_latency(w, self.use_latency);
            let type_str = typ_to_verilog_array(&w.typ);
            write!(self.program_text, "{wire_or_reg}{type_str} {wire_name}")?;

            match &w.source {
                RealWireDataSource::Select { root, path } => {
                    let wire_name = self.wire_name(*root, w.absolute_latency);
                    let path = self.wire_ref_path_to_string(&path, w.absolute_latency);
                    writeln!(self.program_text, " = {wire_name}{path};")?;
                }
                RealWireDataSource::UnaryOp { op, right } => {
                    writeln!(self.program_text, " = {}{};", op.op_text(), self.wire_name(*right, w.absolute_latency))?;
                }
                RealWireDataSource::BinaryOp { op, left, right } => {
                    writeln!(self.program_text, " = {} {} {};", self.wire_name(*left, w.absolute_latency), op.op_text(), self.wire_name(*right, w.absolute_latency))?;
                }
                RealWireDataSource::Constant { value } => {
                    writeln!(self.program_text, " = {};", value.to_string())?;
                }
                RealWireDataSource::ReadOnly | RealWireDataSource::OutPort { sub_module_id:_, port_id:_ } => {
                    writeln!(self.program_text, ";")?;
                }
                RealWireDataSource::Multiplexer{is_state, sources : _} => {
                    writeln!(self.program_text, ";")?;
                    if let Some(initial_value) = is_state {
                        if initial_value.is_valid() {
                            let initial_value_str = initial_value.to_string();
                            writeln!(self.program_text, "initial {wire_name} = {initial_value_str};")?;
                        }
                    }
                }
            }
            self.add_latency_registers(w)?;
        }
        
        // Output all submodules
        for (_id, sm) in &self.instance.submodules {
            let sm_inst : &InstantiatedModule = sm.instance.as_ref().expect("Invalid submodules are impossible to remain by the time codegen happens");
            let sm_instance_name = mangle(&sm_inst.name);
            let sm_name = &sm.name;
            writeln!(self.program_text, "{sm_instance_name} {sm_name}(")?;
            writeln!(self.program_text, "\t.clk(clk),")?;
            for (port_id, iport) in sm_inst.interface_ports.iter_valids() {
                let port_name = wire_name_self_latency(&sm_inst.wires[iport.wire], self.use_latency);
                let wire_name = if let Some(port_wire) = &sm.port_map[port_id] {
                    wire_name_self_latency(&self.instance.wires[port_wire.maps_to_wire], self.use_latency)
                } else {
                    // Ports that are defined on the submodule, but not used by impl
                    String::new()
                };
                writeln!(self.program_text, "\t.{port_name}({wire_name}),")?;
            }
            writeln!(self.program_text, ");")?;
        }

        // For multiplexers, output 
        for (_id, w) in &self.instance.wires {
            match &w.source {
                RealWireDataSource::Multiplexer{is_state, sources} => {
                    let output_name = wire_name_self_latency(w, self.use_latency);
                    if is_state.is_some() {
                        writeln!(self.program_text, "/*always_ff*/ always @(posedge clk) begin")?;
                    } else {
                        writeln!(self.program_text, "/*always_comb*/ always @(*) begin")?;
                        writeln!(self.program_text, "\t{output_name} <= 1'bX; // Combinatorial wires are not defined when not valid")?;
                    }
                    
                    for s in sources {
                        let path = self.wire_ref_path_to_string(&s.to_path, w.absolute_latency);
                        let from_name = self.wire_name(s.from.from, w.absolute_latency);
                        if let Some(cond) = s.from.condition {
                            let cond_name = self.wire_name(cond, w.absolute_latency);
                            writeln!(self.program_text, "\tif({cond_name}) begin {output_name}{path} <= {from_name}; end")?;
                        } else {
                            writeln!(self.program_text, "\t{output_name}{path} <= {from_name};")?;
                        }
                    }
                    writeln!(self.program_text, "end")?;
                }
                RealWireDataSource::ReadOnly => {}
                RealWireDataSource::OutPort { sub_module_id:_, port_id:_ } => {}
                RealWireDataSource::Select{root : _, path : _} => {}
                RealWireDataSource::UnaryOp{op : _, right : _} => {}
                RealWireDataSource::BinaryOp{op : _, left : _, right : _} => {}
                RealWireDataSource::Constant{value : _} => {}
            }
        }

        writeln!(self.program_text, "endmodule\n")?;

        Ok(())
    }
}

pub fn gen_verilog_code(md : &Module, instance : &InstantiatedModule, use_latency : bool) -> String {
    let mut program_text = String::new();

    let mut ctx = CodeGenerationContext{md, instance, program_text: &mut program_text, use_latency};
    ctx.write_verilog_code().unwrap();

    program_text
}

pub fn mangle(str : &str) -> String {
    let mut result = String::with_capacity(str.len());
    for c in str.chars() {
        if c.is_whitespace() || c == ':' {continue}
        result.push(if c.is_alphanumeric() {
            c
        } else {'_'});
    }
    result
}
