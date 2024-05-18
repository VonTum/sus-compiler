

use std::{cmp::max, iter::zip};

use crate::{
    arena_alloc::{FlatAlloc, UUID},
    flattening::{FlatIDMarker, Instruction, WriteModifiers},
    instantiation::latency_algorithm::{convert_fanin_to_fanout, solve_latencies, FanInOut, LatencyCountingError}
};

use self::list_of_lists::ListOfLists;

use super::*;


struct PathMuxSource<'s> {
    to_wire : &'s RealWire,
    to_latency : i64,
    mux_input : &'s MultiplexerSource
}
fn gather_all_mux_inputs<'w>(wires : &'w FlatAlloc<RealWire, WireIDMarker>, conflict_iter : &[SpecifiedLatency]) -> Vec<PathMuxSource<'w>> {
    let mut connection_list = Vec::new();
    for window in conflict_iter.windows(2) {
        let [from, to] = window else {unreachable!()};
        let from_wire_id = WireID::from_hidden_value(from.wire);
        //let from_wire = &self.wires[from_wire_id];
        let to_wire_id = WireID::from_hidden_value(to.wire);
        let to_wire = &wires[to_wire_id];
        let RealWireDataSource::Multiplexer { is_state:_, sources } = &to_wire.source else {continue}; // We can only name multiplexers

        //let decl_id = to_wire.original_instruction;
        //let Instruction::Declaration(decl) = &self.instructions[decl_id] else {unreachable!()};

        for s in sources {
            let mut predecessor_found = false;
            s.for_each_source(|source| {
                if source == from_wire_id {
                    predecessor_found = true;
                }
            });
            if predecessor_found {
                connection_list.push(PathMuxSource{to_wire, mux_input : s, to_latency : to.latency});
            }
        }
    }
    connection_list
}

fn write_path_elem_to_string(result : &mut String, decl_name : &str, to_absolute_latency : i64, prev_absolute_latency : i64) {
    use std::fmt::Write;

    let delta_latency = to_absolute_latency - prev_absolute_latency;

    let plus_sign = if delta_latency >= 0 {"+"} else {""};

    writeln!(result, "-> {decl_name}'{to_absolute_latency} ({plus_sign}{delta_latency})").unwrap();
}

fn make_path_info_string(writes : &[PathMuxSource<'_>], from_latency : i64, from_name : &str) -> String {
   let mut prev_decl_absolute_latency = from_latency;
    let mut result = format!("{from_name}'{prev_decl_absolute_latency}\n");

    for wr in writes {
        let decl_name = &wr.to_wire.name;

        let to_absolute_latency = wr.to_latency;
        
        write_path_elem_to_string(&mut result, &decl_name, to_absolute_latency, prev_decl_absolute_latency);

        prev_decl_absolute_latency = to_absolute_latency;
    }

    result
}

fn filter_unique_write_flats<'w>(writes : &'w [PathMuxSource<'w>], instructions : &'w FlatAlloc<Instruction, FlatIDMarker>) -> Vec<&'w crate::flattening::Write> {
    let mut result : Vec<&'w crate::flattening::Write> = Vec::new();
    for w in writes {
        let original_write = instructions[w.mux_input.from.original_connection].unwrap_write();
        
        if !result.iter().any(|found_write| std::ptr::eq(*found_write, original_write)) {result.push(original_write)}
    }
    result
}


impl<'fl, 'l> InstantiationContext<'fl, 'l> {
    fn make_fanins(&self) -> (ListOfLists<FanInOut>, Vec<SpecifiedLatency>) {
        let mut fanins : ListOfLists<FanInOut> = ListOfLists::new_with_groups_capacity(self.wires.len());
        let mut initial_latencies = Vec::new();
        
        // Wire to wire Fanin
        for (id, wire) in &self.wires {
            fanins.new_group();
            wire.source.iter_sources_with_min_latency(|from, delta_latency| {
                fanins.push_to_last_group(FanInOut{other : from.get_hidden_value(), delta_latency});
            });

            // Submodules Fanin
            // This creates two way connections, from any input i to output o it creates a |o| - |i| length connection, and a -(|o| - |i|) backward connection. This fixes them to be an exact latency apart. 
            // This is O(lots) but doesn't matter, usually very few submodules. Fix this if needed
            for (_id, sub_mod) in &self.submodules {
                for (port_id, self_wire) in &sub_mod.port_map {
                    // Can assign to the wire, too keep in line with ListOfLists build order
                    if *self_wire != id {continue}

                    // Skip non-instantiated ports
                    let Some(port_in_submodule) = &sub_mod.instance.interface_ports[port_id] else {continue};

                    for (other_port_id, other_port_in_submodule) in sub_mod.instance.interface_ports.iter_valids() {
                        if other_port_in_submodule.is_input == !other_port_in_submodule.is_input {
                            // Valid input/output or output/input pair. Apply delta absolute latency

                            let mut delta_latency = other_port_in_submodule.absolute_latency - port_in_submodule.absolute_latency;

                            if port_in_submodule.is_input {
                                delta_latency = -delta_latency;
                            }

                            let other_wire_in_self = sub_mod.port_map[other_port_id];

                            fanins.push_to_last_group(FanInOut{other: other_wire_in_self.get_hidden_value(), delta_latency});
                        }
                    }
                }
            }

            if wire.absolute_latency != CALCULATE_LATENCY_LATER {
                initial_latencies.push(SpecifiedLatency { wire: id.get_hidden_value(), latency: wire.absolute_latency })
            }
        }

        (fanins, initial_latencies)
    }

    // Returns a proper interface if all ports involved did not produce an error. If a port did produce an error then returns None. 
    // Computes all latencies involved
    pub fn compute_latencies(&mut self) {
        let (fanins, initial_latencies) = self.make_fanins();
        
        // Process fanouts
        let fanouts = convert_fanin_to_fanout(&fanins);

        let mut inputs = Vec::new();
        let mut outputs = Vec::new();

        for (_id, p) in self.interface_ports.iter_valids() {
            if p.is_input {
                inputs.push(p.wire.get_hidden_value());
            } else {
                outputs.push(p.wire.get_hidden_value());
            }
        }
        
        match solve_latencies(&fanins, &fanouts, &inputs, &outputs, initial_latencies) {
            Ok(latencies) => {
                for ((_id, wire), lat) in zip(self.wires.iter_mut(), latencies.iter()) {
                    wire.absolute_latency = *lat;
                    if *lat == CALCULATE_LATENCY_LATER {
                        let source_location = self.md.get_instruction_span(wire.original_instruction);
                        self.errors.error(source_location, format!("Latency Counting couldn't reach this node"));
                    }
                }
                Some(())
            }
            Err(err) => {
                match err {
                    LatencyCountingError::NetPositiveLatencyCycle { conflict_path, net_roundtrip_latency } => {
                        let writes_involved = gather_all_mux_inputs(&self.wires, &conflict_path);
                        assert!(!writes_involved.is_empty());
                        let (first_write, later_writes) = writes_involved.split_first().unwrap();
                        let first_write_desired_latency = first_write.to_latency + net_roundtrip_latency;
                        let mut path_message = make_path_info_string(later_writes, first_write.to_latency, &first_write.to_wire.name);
                        write_path_elem_to_string(&mut path_message, &first_write.to_wire.name, first_write_desired_latency, writes_involved.last().unwrap().to_latency);
                        let unique_write_instructions = filter_unique_write_flats(&writes_involved, &self.md.instructions);
                        let rest_of_message = format!(" part of a net-positive latency cycle of +{net_roundtrip_latency}\n\n{path_message}\nWhich conflicts with the starting latency");
                        
                        let mut did_place_error = false;
                        for wr in &unique_write_instructions {
                            match wr.write_modifiers {
                                WriteModifiers::Connection { num_regs, regs_span } => {
                                    if num_regs >= 1 {
                                        did_place_error = true;
                                        let this_register_plural = if num_regs == 1 {"This register is"} else {"These registers are"};
                                        self.errors.error(regs_span, format!("{this_register_plural}{rest_of_message}"));
                                    }
                                }
                                WriteModifiers::Initial{initial_kw_span : _} => {unreachable!("Initial assignment can only be from compile-time constant. Cannot be part of latency loop. ")}
                            }
                        }
                        // Fallback if no register annotations used
                        if !did_place_error {
                            for wr in unique_write_instructions {
                                self.errors.error(wr.to.span, format!("This write is{rest_of_message}"));
                            }
                        }
                    }
                    LatencyCountingError::IndeterminablePortLatency { bad_ports } => {
                        for port in bad_ports {
                            let port_decl = self.md.instructions[self.wires[WireID::from_hidden_value(port.0)].original_instruction].unwrap_wire_declaration();
                            self.errors.error(port_decl.name_span, format!("Cannot determine port latency. Options are {} and {}\nTry specifying an explicit latency or rework the module to remove this ambiguity", port.1, port.2));
                        }
                    }
                    LatencyCountingError::ConflictingSpecifiedLatencies { conflict_path } => {
                        let start_wire = &self.wires[WireID::from_hidden_value(conflict_path.first().unwrap().wire)];
                        let end_wire = &self.wires[WireID::from_hidden_value(conflict_path.last().unwrap().wire)];
                        let start_decl = self.md.instructions[start_wire.original_instruction].unwrap_wire_declaration();
                        let end_decl = self.md.instructions[end_wire.original_instruction].unwrap_wire_declaration();
                        let end_latency_decl = self.md.instructions[end_decl.latency_specifier.unwrap()].unwrap_wire();
                        

                        let writes_involved = gather_all_mux_inputs(&self.wires, &conflict_path);
                        let path_message = make_path_info_string(&writes_involved, start_wire.absolute_latency, &start_wire.name);
                        //assert!(!writes_involved.is_empty());

                        let end_name = &end_wire.name;
                        let specified_end_latency = end_wire.absolute_latency;
                        self.errors
                            .error(end_latency_decl.span, format!("Conflicting specified latency\n\n{path_message}\nBut this was specified as {end_name}'{specified_end_latency}"))
                            .info_obj_same_file(start_decl);
                    }
                }
                None
            }
        };

        // Compute needed_untils
        for id in self.wires.id_range() {
            let wire = &self.wires[id];
            let mut needed_until = wire.absolute_latency;
            for target_fanout in &fanouts[id.get_hidden_value()] {
                let target_wire = &self.wires[UUID::from_hidden_value(target_fanout.other)];

                needed_until = max(needed_until, target_wire.absolute_latency);
            }
            self.wires[id].needed_until = needed_until;
        }

        // Finally update interface absolute latencies
        for (_id, port) in self.interface_ports.iter_valids_mut() {
            port.absolute_latency = self.wires[port.wire].absolute_latency;
        }
    }
}
