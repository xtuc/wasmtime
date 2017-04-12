//! Data flow graph tracking Instructions, Values, and EBBs.

use ir::{Ebb, Inst, Value, Type, SigRef, Signature, FuncRef, ValueList, ValueListPool};
use ir::entities::ExpandedValue;
use ir::instructions::{Opcode, InstructionData, CallInfo};
use ir::extfunc::ExtFuncData;
use entity_map::{EntityMap, PrimaryEntityData};
use ir::builder::{InsertBuilder, ReplaceBuilder, InstBuilder};
use ir::layout::Cursor;
use packed_option::PackedOption;
use write::write_operands;

use std::fmt;
use std::ops::{Index, IndexMut};
use std::u16;

/// A data flow graph defines all instructions and extended basic blocks in a function as well as
/// the data flow dependencies between them. The DFG also tracks values which can be either
/// instruction results or EBB arguments.
///
/// The layout of EBBs in the function and of instructions in each EBB is recorded by the
/// `FunctionLayout` data structure which form the other half of the function representation.
///
#[derive(Clone)]
pub struct DataFlowGraph {
    /// Data about all of the instructions in the function, including opcodes and operands.
    /// The instructions in this map are not in program order. That is tracked by `Layout`, along
    /// with the EBB containing each instruction.
    insts: EntityMap<Inst, InstructionData>,

    /// List of result values for each instruction.
    ///
    /// This map gets resized automatically by `make_inst()` so it is always in sync with the
    /// primary `insts` map.
    results: EntityMap<Inst, ValueList>,

    /// Extended basic blocks in the function and their arguments.
    /// This map is not in program order. That is handled by `Layout`, and so is the sequence of
    /// instructions contained in each EBB.
    ebbs: EntityMap<Ebb, EbbData>,

    /// Memory pool of value lists.
    ///
    /// The `ValueList` references into this pool appear in many places:
    ///
    /// - Instructions in `insts` that don't have room for their entire argument list inline.
    /// - Instruction result values in `results`.
    /// - EBB arguments in `ebbs`.
    pub value_lists: ValueListPool,

    /// Extended value table. Most `Value` references refer directly to their defining instruction.
    /// Others index into this table.
    ///
    /// This is implemented directly with a `Vec` rather than an `EntityMap<Value, ...>` because
    /// the Value entity references can refer to two things -- an instruction or an extended value.
    extended_values: Vec<ValueData>,

    /// Function signature table. These signatures are referenced by indirect call instructions as
    /// well as the external function references.
    pub signatures: EntityMap<SigRef, Signature>,

    /// External function references. These are functions that can be called directly.
    pub ext_funcs: EntityMap<FuncRef, ExtFuncData>,
}

impl PrimaryEntityData for InstructionData {}
impl PrimaryEntityData for EbbData {}
impl PrimaryEntityData for Signature {}
impl PrimaryEntityData for ExtFuncData {}

impl DataFlowGraph {
    /// Create a new empty `DataFlowGraph`.
    pub fn new() -> DataFlowGraph {
        DataFlowGraph {
            insts: EntityMap::new(),
            results: EntityMap::new(),
            ebbs: EntityMap::new(),
            value_lists: ValueListPool::new(),
            extended_values: Vec::new(),
            signatures: EntityMap::new(),
            ext_funcs: EntityMap::new(),
        }
    }

    /// Get the total number of instructions created in this function, whether they are currently
    /// inserted in the layout or not.
    ///
    /// This is intended for use with `EntityMap::with_capacity`.
    pub fn num_insts(&self) -> usize {
        self.insts.len()
    }

    /// Returns `true` if the given instruction reference is valid.
    pub fn inst_is_valid(&self, inst: Inst) -> bool {
        self.insts.is_valid(inst)
    }

    /// Get the total number of extended basic blocks created in this function, whether they are
    /// currently inserted in the layout or not.
    ///
    /// This is intended for use with `EntityMap::with_capacity`.
    pub fn num_ebbs(&self) -> usize {
        self.ebbs.len()
    }

    /// Returns `true` if the given ebb reference is valid.
    pub fn ebb_is_valid(&self, ebb: Ebb) -> bool {
        self.ebbs.is_valid(ebb)
    }
}

/// Handling values.
///
/// Values are either EBB arguments or instruction results.
impl DataFlowGraph {
    // Allocate an extended value entry.
    fn make_value(&mut self, data: ValueData) -> Value {
        let vref = Value::new_table(self.extended_values.len());
        self.extended_values.push(data);
        vref
    }

    /// Check if a value reference is valid.
    pub fn value_is_valid(&self, v: Value) -> bool {
        match v.expand() {
            ExpandedValue::Direct(inst) => self.insts.is_valid(inst),
            ExpandedValue::Table(index) => index < self.extended_values.len(),
        }
    }

    /// Get the type of a value.
    pub fn value_type(&self, v: Value) -> Type {
        use ir::entities::ExpandedValue::*;
        match v.expand() {
            Direct(i) => self.insts[i].first_type(),
            Table(i) => {
                match self.extended_values[i] {
                    ValueData::Inst { ty, .. } => ty,
                    ValueData::Arg { ty, .. } => ty,
                    ValueData::Alias { ty, .. } => ty,
                }
            }
        }
    }

    /// Get the definition of a value.
    ///
    /// This is either the instruction that defined it or the Ebb that has the value as an
    /// argument.
    pub fn value_def(&self, v: Value) -> ValueDef {
        use ir::entities::ExpandedValue::*;
        match v.expand() {
            Direct(inst) => ValueDef::Res(inst, 0),
            Table(idx) => {
                match self.extended_values[idx] {
                    ValueData::Inst { inst, num, .. } => {
                        assert_eq!(Some(v),
                                   self.results[inst].get(num as usize, &self.value_lists),
                                   "Dangling result value {}: {}",
                                   v,
                                   self.display_inst(inst));
                        ValueDef::Res(inst, num as usize)
                    }
                    ValueData::Arg { ebb, num, .. } => {
                        assert_eq!(Some(v),
                                   self.ebbs[ebb].args.get(num as usize, &self.value_lists),
                                   "Dangling EBB argument value");
                        ValueDef::Arg(ebb, num as usize)
                    }
                    ValueData::Alias { original, .. } => {
                        // Make sure we only recurse one level. `resolve_aliases` has safeguards to
                        // detect alias loops without overrunning the stack.
                        self.value_def(self.resolve_aliases(original))
                    }
                }
            }
        }
    }

    /// Resolve value aliases.
    ///
    /// Find the original SSA value that `value` aliases.
    pub fn resolve_aliases(&self, value: Value) -> Value {
        use ir::entities::ExpandedValue::Table;
        let mut v = value;

        // Note that extended_values may be empty here.
        for _ in 0..1 + self.extended_values.len() {
            v = match v.expand() {
                Table(idx) => {
                    match self.extended_values[idx] {
                        ValueData::Alias { original, .. } => {
                            // Follow alias values.
                            original
                        }
                        _ => return v,
                    }
                }
                _ => return v,
            };
        }
        panic!("Value alias loop detected for {}", value);
    }

    /// Resolve value copies.
    ///
    /// Find the original definition of a value, looking through value aliases as well as
    /// copy/spill/fill instructions.
    pub fn resolve_copies(&self, value: Value) -> Value {
        use ir::entities::ExpandedValue::Direct;
        let mut v = value;

        for _ in 0..self.insts.len() {
            v = self.resolve_aliases(v);
            v = match v.expand() {
                Direct(inst) => {
                    match self[inst] {
                        InstructionData::Unary { opcode, arg, .. } => {
                            match opcode {
                                Opcode::Copy | Opcode::Spill | Opcode::Fill => arg,
                                _ => return v,
                            }
                        }
                        _ => return v,
                    }
                }
                _ => return v,
            };
        }
        panic!("Copy loop detected for {}", value);
    }

    /// Turn a value into an alias of another.
    ///
    /// Change the `dest` value to behave as an alias of `src`. This means that all uses of `dest`
    /// will behave as if they used that value `src`.
    ///
    /// The `dest` value cannot be a direct value defined as the first result of an instruction. To
    /// replace a direct value with `src`, its defining instruction should be replaced with a
    /// `copy src` instruction. See `replace()`.
    pub fn change_to_alias(&mut self, dest: Value, src: Value) {
        use ir::entities::ExpandedValue::Table;

        // Try to create short alias chains by finding the original source value.
        // This also avoids the creation of loops.
        let original = self.resolve_aliases(src);
        assert!(dest != original,
                "Aliasing {} to {} would create a loop",
                dest,
                src);
        let ty = self.value_type(original);
        assert_eq!(self.value_type(dest),
                   ty,
                   "Aliasing {} to {} would change its type {} to {}",
                   dest,
                   src,
                   self.value_type(dest),
                   ty);

        if let Table(idx) = dest.expand() {
            self.extended_values[idx] = ValueData::Alias {
                ty: ty,
                original: original,
            };
        } else {
            panic!("Cannot change direct value {} into an alias", dest);
        }
    }

    /// Create a new value alias.
    ///
    /// Note that this function should only be called by the parser.
    pub fn make_value_alias(&mut self, src: Value) -> Value {
        let ty = self.value_type(src);

        let data = ValueData::Alias {
            ty: ty,
            original: src,
        };
        self.make_value(data)
    }
}

/// Where did a value come from?
#[derive(Debug, PartialEq, Eq)]
pub enum ValueDef {
    /// Value is the n'th result of an instruction.
    Res(Inst, usize),
    /// Value is the n'th argument to an EBB.
    Arg(Ebb, usize),
}

// Internal table storage for extended values.
#[derive(Clone, Debug)]
enum ValueData {
    // Value is defined by an instruction, but it is not the first result.
    Inst {
        ty: Type,
        num: u16, // Result number starting from 0.
        inst: Inst,
        next: PackedOption<Value>, // Next result defined by `def`.
    },

    // Value is an EBB argument.
    Arg {
        ty: Type,
        num: u16, // Argument number, starting from 0.
        ebb: Ebb,
    },

    // Value is an alias of another value.
    // An alias value can't be linked as an instruction result or EBB argument. It is used as a
    // placeholder when the original instruction or EBB has been rewritten or modified.
    Alias { ty: Type, original: Value },
}

/// Instructions.
///
impl DataFlowGraph {
    /// Create a new instruction.
    ///
    /// The type of the first result is indicated by `data.ty`. If the instruction produces
    /// multiple results, also call `make_inst_results` to allocate value table entries.
    pub fn make_inst(&mut self, data: InstructionData) -> Inst {
        let n = self.num_insts() + 1;
        self.results.resize(n);
        self.insts.push(data)
    }

    /// Get the instruction reference that will be assigned to the next instruction created by
    /// `make_inst`.
    ///
    /// This is only really useful to the parser.
    pub fn next_inst(&self) -> Inst {
        self.insts.next_key()
    }

    /// Returns an object that displays `inst`.
    pub fn display_inst(&self, inst: Inst) -> DisplayInst {
        DisplayInst(self, inst)
    }

    /// Get all value arguments on `inst` as a slice.
    pub fn inst_args(&self, inst: Inst) -> &[Value] {
        self.insts[inst].arguments(&self.value_lists)
    }

    /// Get all value arguments on `inst` as a mutable slice.
    pub fn inst_args_mut(&mut self, inst: Inst) -> &mut [Value] {
        self.insts[inst].arguments_mut(&mut self.value_lists)
    }

    /// Get the fixed value arguments on `inst` as a slice.
    pub fn inst_fixed_args(&self, inst: Inst) -> &[Value] {
        let fixed_args = self[inst]
            .opcode()
            .constraints()
            .fixed_value_arguments();
        &self.inst_args(inst)[..fixed_args]
    }

    /// Get the fixed value arguments on `inst` as a mutable slice.
    pub fn inst_fixed_args_mut(&mut self, inst: Inst) -> &mut [Value] {
        let fixed_args = self[inst]
            .opcode()
            .constraints()
            .fixed_value_arguments();
        &mut self.inst_args_mut(inst)[..fixed_args]
    }

    /// Get the variable value arguments on `inst` as a slice.
    pub fn inst_variable_args(&self, inst: Inst) -> &[Value] {
        let fixed_args = self[inst]
            .opcode()
            .constraints()
            .fixed_value_arguments();
        &self.inst_args(inst)[fixed_args..]
    }

    /// Get the variable value arguments on `inst` as a mutable slice.
    pub fn inst_variable_args_mut(&mut self, inst: Inst) -> &mut [Value] {
        let fixed_args = self[inst]
            .opcode()
            .constraints()
            .fixed_value_arguments();
        &mut self.inst_args_mut(inst)[fixed_args..]
    }

    /// Create result values for an instruction that produces multiple results.
    ///
    /// Instructions that produce no result values only need to be created with `make_inst`,
    /// otherwise call `make_inst_results` to allocate value table entries for the results.
    ///
    /// The result value types are determined from the instruction's value type constraints and the
    /// provided `ctrl_typevar` type for polymorphic instructions. For non-polymorphic
    /// instructions, `ctrl_typevar` is ignored, and `VOID` can be used.
    ///
    /// The type of the first result value is also set, even if it was already set in the
    /// `InstructionData` passed to `make_inst`. If this function is called with a single-result
    /// instruction, that is the only effect.
    ///
    /// Returns the number of results produced by the instruction.
    pub fn make_inst_results(&mut self, inst: Inst, ctrl_typevar: Type) -> usize {
        let constraints = self.insts[inst].opcode().constraints();
        let fixed_results = constraints.fixed_results();
        let mut total_results = fixed_results;

        self.results[inst].clear(&mut self.value_lists);

        // Additional values form a linked list starting from the second result value. Generate
        // the list backwards so we don't have to modify value table entries in place. (This
        // causes additional result values to be numbered backwards which is not the aesthetic
        // choice, but since it is only visible in extremely rare instructions with 3+ results,
        // we don't care).
        let mut head = None;
        let mut first_type = None;
        let mut rev_num = 1;

        // Get the call signature if this is a function call.
        if let Some(sig) = self.call_signature(inst) {
            // Create result values corresponding to the call return types.
            let var_results = self.signatures[sig].return_types.len();
            total_results += var_results;

            for res_idx in (0..var_results).rev() {
                if let Some(ty) = first_type {
                    head = Some(self.make_value(ValueData::Inst {
                                                    ty: ty,
                                                    num: (total_results - rev_num) as u16,
                                                    inst: inst,
                                                    next: head.into(),
                                                }));
                    self.results[inst].push(head.unwrap(), &mut self.value_lists);
                    rev_num += 1;
                }
                first_type = Some(self.signatures[sig].return_types[res_idx].value_type);
            }
        }

        // Then the fixed results which will appear at the front of the list.
        for res_idx in (0..fixed_results).rev() {
            if let Some(ty) = first_type {
                head = Some(self.make_value(ValueData::Inst {
                                                ty: ty,
                                                num: (total_results - rev_num) as u16,
                                                inst: inst,
                                                next: head.into(),
                                            }));
                rev_num += 1;
                self.results[inst].push(head.unwrap(), &mut self.value_lists);
            }
            first_type = Some(constraints.result_type(res_idx, ctrl_typevar));
        }

        *self.insts[inst].first_type_mut() = first_type.unwrap_or_default();

        // Include the first result in the results vector.
        if first_type.is_some() {
            self.results[inst].push(Value::new_direct(inst), &mut self.value_lists);
        }
        self.results[inst]
            .as_mut_slice(&mut self.value_lists)
            .reverse();

        total_results
    }

    /// Create an `InsertBuilder` that will insert an instruction at the cursor's current position.
    pub fn ins<'c, 'fc: 'c, 'fd>(&'fd mut self,
                                 at: &'c mut Cursor<'fc>)
                                 -> InsertBuilder<'c, 'fc, 'fd> {
        InsertBuilder::new(self, at)
    }

    /// Create a `ReplaceBuilder` that will replace `inst` with a new instruction in place.
    pub fn replace(&mut self, inst: Inst) -> ReplaceBuilder {
        ReplaceBuilder::new(self, inst)
    }

    /// Detach secondary instruction results, and return the first of them.
    ///
    /// If `inst` produces two or more results, detach these secondary result values from `inst`.
    /// The first result value cannot be detached.
    /// The full list of secondary results can be traversed with `next_secondary_result()`.
    ///
    /// Use this method to detach secondary values before using `replace(inst)` to provide an
    /// alternate instruction for computing the primary result value.
    pub fn detach_secondary_results(&mut self, inst: Inst) -> Option<Value> {
        if !self.has_results(inst) {
            return None;
        }

        let second = self.results[inst].get(1, &mut self.value_lists);
        self.results[inst].clear(&mut self.value_lists);
        if !self.insts[inst].first_type().is_void() {
            self.results[inst].push(Value::new_direct(inst), &mut self.value_lists);
        }
        second
    }

    /// Get the next secondary result after `value`.
    ///
    /// Use this function to traverse the full list of instruction results returned from
    /// `detach_secondary_results()`.
    pub fn next_secondary_result(&self, value: Value) -> Option<Value> {
        if let ExpandedValue::Table(index) = value.expand() {
            if let ValueData::Inst { next, .. } = self.extended_values[index] {
                return next.into();
            }
        }
        panic!("{} is not a secondary result value", value);
    }

    /// Attach an existing value as a secondary result after `last_res` which must be the last
    /// result of an instruction.
    ///
    /// This is a very low-level operation. Usually, instruction results with the correct types are
    /// created automatically. The `res` value must be a secondary instruction result detached from
    /// somewhere else.
    pub fn attach_secondary_result(&mut self, last_res: Value, res: Value) {
        let res_inst = match last_res.expand() {
            ExpandedValue::Direct(inst) => inst,
            ExpandedValue::Table(idx) => {
                if let ValueData::Inst { inst, ref mut next, .. } = self.extended_values[idx] {
                    assert!(next.is_none(), "last_res is not the last result");
                    *next = res.into();
                    inst
                } else {
                    panic!("last_res is not an instruction result");
                }
            }
        };

        let res_num = self.results[res_inst].push(res, &mut self.value_lists);
        assert!(res_num <= u16::MAX as usize, "Too many result values");

        // Now update `res` itself.
        if let ExpandedValue::Table(idx) = res.expand() {
            if let ValueData::Inst {
                       ref mut num,
                       ref mut inst,
                       ref mut next,
                       ..
                   } = self.extended_values[idx] {
                *num = res_num as u16;
                *inst = res_inst;
                *next = None.into();
                return;
            }
        }
        panic!("{} must be a result", res);
    }

    /// Append a new instruction result value after `last_res`.
    ///
    /// The `last_res` value must be the last value on an instruction.
    pub fn append_secondary_result(&mut self, last_res: Value, ty: Type) -> Value {
        // The only member that matters is `ty`. The rest is filled in by
        // `attach_secondary_result`.
        use entity_map::EntityRef;
        let res = self.make_value(ValueData::Inst {
                                      ty: ty,
                                      inst: Inst::new(0),
                                      num: 0,
                                      next: None.into(),
                                  });
        self.attach_secondary_result(last_res, res);
        res
    }

    /// Move the instruction at `pos` to a new `Inst` reference so its first result can be
    /// redefined without overwriting the original instruction.
    ///
    /// The first result value of an instruction is intrinsically tied to the `Inst` reference, so
    /// it is not possible to detach the value and attach it to something else. This function
    /// copies the instruction pointed to by `pos` to a new `Inst` reference, making the original
    /// `Inst` reference available to be redefined with `dfg.replace(inst)` above.
    ///
    /// Before:
    ///
    ///   inst1:  v1, vx2 = foo  <-- pos
    ///
    /// After:
    ///
    ///   inst7:  v7, vx2 = foo
    ///   inst1:  v1 = copy v7   <-- pos
    ///
    /// Returns the new `Inst` reference where the original instruction has been moved.
    pub fn redefine_first_value(&mut self, pos: &mut Cursor) -> Inst {
        let orig = pos.current_inst()
            .expect("Cursor must point at an instruction");
        let data = self[orig].clone();
        // After cloning, any secondary values are attached to both copies. Don't do that, we only
        // want them on the new clone.
        let mut results = self.results[orig].take();
        self.detach_secondary_results(orig);
        let new = self.make_inst(data);
        results.as_mut_slice(&mut self.value_lists)[0] = Value::new_direct(new);
        self.results[new] = results;
        pos.insert_inst(new);
        // Replace the original instruction with a copy of the new value.
        // This is likely to be immediately overwritten by something else, but this way we avoid
        // leaving the DFG in a state with multiple references to secondary results and value
        // lists. It also means that this method doesn't change the semantics of the program.
        let new_value = self.first_result(new);
        self.replace(orig).copy(new_value);
        new
    }

    /// Get the first result of an instruction.
    ///
    /// This function panics if the instruction doesn't have any result.
    pub fn first_result(&self, inst: Inst) -> Value {
        self.results[inst]
            .first(&self.value_lists)
            .expect("Instruction has no results")
    }

    /// Test if `inst` has any result values currently.
    pub fn has_results(&self, inst: Inst) -> bool {
        !self.results[inst].is_empty()
    }

    /// Return all the results of an instruction.
    pub fn inst_results(&self, inst: Inst) -> &[Value] {
        self.results[inst].as_slice(&self.value_lists)
    }

    /// Get the call signature of a direct or indirect call instruction.
    /// Returns `None` if `inst` is not a call instruction.
    pub fn call_signature(&self, inst: Inst) -> Option<SigRef> {
        match self.insts[inst].analyze_call(&self.value_lists) {
            CallInfo::NotACall => None,
            CallInfo::Direct(f, _) => Some(self.ext_funcs[f].signature),
            CallInfo::Indirect(s, _) => Some(s),
        }
    }

    /// Compute the type of an instruction result from opcode constraints and call signatures.
    ///
    /// This computes the same sequence of result types that `make_inst_results()` above would
    /// assign to the created result values, but it does not depend on `make_inst_results()` being
    /// called first.
    ///
    /// Returns `None` if asked about a result index that is too large.
    pub fn compute_result_type(&self,
                               inst: Inst,
                               result_idx: usize,
                               ctrl_typevar: Type)
                               -> Option<Type> {
        let constraints = self.insts[inst].opcode().constraints();
        let fixed_results = constraints.fixed_results();

        if result_idx < fixed_results {
            return Some(constraints.result_type(result_idx, ctrl_typevar));
        }

        // Not a fixed result, try to extract a return type from the call signature.
        self.call_signature(inst)
            .and_then(|sigref| {
                          self.signatures[sigref]
                              .return_types
                              .get(result_idx - fixed_results)
                              .map(|&arg| arg.value_type)
                      })
    }
}

/// Allow immutable access to instructions via indexing.
impl Index<Inst> for DataFlowGraph {
    type Output = InstructionData;

    fn index<'a>(&'a self, inst: Inst) -> &'a InstructionData {
        &self.insts[inst]
    }
}

/// Allow mutable access to instructions via indexing.
impl IndexMut<Inst> for DataFlowGraph {
    fn index_mut<'a>(&'a mut self, inst: Inst) -> &'a mut InstructionData {
        &mut self.insts[inst]
    }
}

/// Extended basic blocks.
impl DataFlowGraph {
    /// Create a new basic block.
    pub fn make_ebb(&mut self) -> Ebb {
        self.ebbs.push(EbbData::new())
    }

    /// Get the number of arguments on `ebb`.
    pub fn num_ebb_args(&self, ebb: Ebb) -> usize {
        self.ebbs[ebb].args.len(&self.value_lists)
    }

    /// Append an argument with type `ty` to `ebb`.
    pub fn append_ebb_arg(&mut self, ebb: Ebb, ty: Type) -> Value {
        let val = self.make_value(ValueData::Arg {
                                      ty: ty,
                                      ebb: ebb,
                                      num: 0,
                                  });
        self.attach_ebb_arg(ebb, val);
        val
    }

    /// Get the arguments to an EBB.
    pub fn ebb_args(&self, ebb: Ebb) -> &[Value] {
        self.ebbs[ebb].args.as_slice(&self.value_lists)
    }

    /// Replace an EBB argument with a new value of type `ty`.
    ///
    /// The `old_value` must be an attached EBB argument. It is removed from its place in the list
    /// of arguments and replaced by a new value of type `new_type`. The new value gets the same
    /// position in the list, and other arguments are not disturbed.
    ///
    /// The old value is left detached, so it should probably be changed into something else.
    ///
    /// Returns the new value.
    pub fn replace_ebb_arg(&mut self, old_arg: Value, new_type: Type) -> Value {
        let old_data = if let ExpandedValue::Table(index) = old_arg.expand() {
            self.extended_values[index].clone()
        } else {
            panic!("old_arg: {} must be an EBB argument", old_arg);
        };

        // Create new value identical to the old one except for the type.
        let (ebb, num) = if let ValueData::Arg { num, ebb, .. } = old_data {
            (ebb, num)
        } else {
            panic!("old_arg: {} must be an EBB argument: {:?}",
                   old_arg,
                   old_data);
        };
        let new_arg = self.make_value(ValueData::Arg {
                                          ty: new_type,
                                          num: num,
                                          ebb: ebb,
                                      });

        self.ebbs[ebb].args.as_mut_slice(&mut self.value_lists)[num as usize] = new_arg;
        new_arg
    }

    /// Detach all the arguments from `ebb` and return them as a `ValueList`.
    ///
    /// This is a quite low-level operation. Sensible things to do with the detached EBB arguments
    /// is to put them back on the same EBB with `attach_ebb_arg()` or change them into aliases
    /// with `change_to_alias()`.
    pub fn detach_ebb_args(&mut self, ebb: Ebb) -> ValueList {
        self.ebbs[ebb].args.take()
    }

    /// Append an existing argument value to `ebb`.
    ///
    /// The appended value should already be an EBB argument belonging to `ebb`, but it can't be
    /// attached. In practice, this means that it should be one of the values returned from
    /// `detach_ebb_args()`.
    ///
    /// In almost all cases, you should be using `append_ebb_arg()` instead of this method.
    pub fn attach_ebb_arg(&mut self, ebb: Ebb, arg: Value) {
        let arg_num = self.ebbs[ebb].args.push(arg, &mut self.value_lists);
        assert!(arg_num <= u16::MAX as usize, "Too many arguments to EBB");

        // Now update `arg` itself.
        let arg_ebb = ebb;
        if let ExpandedValue::Table(idx) = arg.expand() {
            if let ValueData::Arg { ref mut num, ebb, .. } = self.extended_values[idx] {
                *num = arg_num as u16;
                assert_eq!(arg_ebb, ebb, "{} should already belong to EBB", arg);
                return;
            }
        }
        panic!("{} must be an EBB argument value", arg);
    }
}

// Contents of an extended basic block.
//
// Arguments for an extended basic block are values that dominate everything in the EBB. All
// branches to this EBB must provide matching arguments, and the arguments to the entry EBB must
// match the function arguments.
#[derive(Clone)]
struct EbbData {
    // List of arguments to this EBB.
    args: ValueList,
}

impl EbbData {
    fn new() -> EbbData {
        EbbData { args: ValueList::new() }
    }
}

/// Object that can display an instruction.
pub struct DisplayInst<'a>(&'a DataFlowGraph, Inst);

impl<'a> fmt::Display for DisplayInst<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dfg = self.0;
        let inst = &dfg[self.1];

        if let Some((first, rest)) = dfg.inst_results(self.1).split_first() {
            write!(f, "{}", first)?;
            for v in rest {
                write!(f, ", {}", v)?;
            }
            write!(f, " = ")?;
        }


        let typevar = inst.ctrl_typevar(dfg);
        if typevar.is_void() {
            write!(f, "{}", inst.opcode())?;
        } else {
            write!(f, "{}.{}", inst.opcode(), typevar)?;
        }
        write_operands(f, dfg, self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ir::types;
    use ir::{Function, Cursor, Opcode, InstructionData};

    #[test]
    fn make_inst() {
        let mut dfg = DataFlowGraph::new();

        let idata = InstructionData::Nullary {
            opcode: Opcode::Iconst,
            ty: types::VOID,
        };
        let inst = dfg.make_inst(idata);
        dfg.make_inst_results(inst, types::I32);
        assert_eq!(inst.to_string(), "inst0");
        assert_eq!(dfg.display_inst(inst).to_string(), "v0 = iconst.i32");

        // Immutable reference resolution.
        {
            let immdfg = &dfg;
            let ins = &immdfg[inst];
            assert_eq!(ins.opcode(), Opcode::Iconst);
            assert_eq!(ins.first_type(), types::I32);
        }

        // Results.
        let val = dfg.first_result(inst);
        assert_eq!(dfg.inst_results(inst), &[val]);

        assert_eq!(dfg.value_def(val), ValueDef::Res(inst, 0));
        assert_eq!(dfg.value_type(val), types::I32);
    }

    #[test]
    fn no_results() {
        let mut dfg = DataFlowGraph::new();

        let idata = InstructionData::Nullary {
            opcode: Opcode::Trap,
            ty: types::VOID,
        };
        let inst = dfg.make_inst(idata);
        assert_eq!(dfg.display_inst(inst).to_string(), "trap");

        // Result slice should be empty.
        assert_eq!(dfg.inst_results(inst), &[]);
    }

    #[test]
    fn ebb() {
        let mut dfg = DataFlowGraph::new();

        let ebb = dfg.make_ebb();
        assert_eq!(ebb.to_string(), "ebb0");
        assert_eq!(dfg.num_ebb_args(ebb), 0);
        assert_eq!(dfg.ebb_args(ebb), &[]);
        assert!(dfg.detach_ebb_args(ebb).is_empty());
        assert_eq!(dfg.num_ebb_args(ebb), 0);
        assert_eq!(dfg.ebb_args(ebb), &[]);

        let arg1 = dfg.append_ebb_arg(ebb, types::F32);
        assert_eq!(arg1.to_string(), "vx0");
        assert_eq!(dfg.num_ebb_args(ebb), 1);
        assert_eq!(dfg.ebb_args(ebb), &[arg1]);

        let arg2 = dfg.append_ebb_arg(ebb, types::I16);
        assert_eq!(arg2.to_string(), "vx1");
        assert_eq!(dfg.num_ebb_args(ebb), 2);
        assert_eq!(dfg.ebb_args(ebb), &[arg1, arg2]);

        assert_eq!(dfg.value_def(arg1), ValueDef::Arg(ebb, 0));
        assert_eq!(dfg.value_def(arg2), ValueDef::Arg(ebb, 1));
        assert_eq!(dfg.value_type(arg1), types::F32);
        assert_eq!(dfg.value_type(arg2), types::I16);

        // Swap the two EBB arguments.
        let vlist = dfg.detach_ebb_args(ebb);
        assert_eq!(dfg.num_ebb_args(ebb), 0);
        assert_eq!(dfg.ebb_args(ebb), &[]);
        assert_eq!(vlist.as_slice(&dfg.value_lists), &[arg1, arg2]);
        dfg.attach_ebb_arg(ebb, arg2);
        let arg3 = dfg.append_ebb_arg(ebb, types::I32);
        dfg.attach_ebb_arg(ebb, arg1);
        assert_eq!(dfg.ebb_args(ebb), &[arg2, arg3, arg1]);
    }

    #[test]
    fn replace_ebb_arguments() {
        let mut dfg = DataFlowGraph::new();

        let ebb = dfg.make_ebb();
        let arg1 = dfg.append_ebb_arg(ebb, types::F32);

        let new1 = dfg.replace_ebb_arg(arg1, types::I64);
        assert_eq!(dfg.value_type(arg1), types::F32);
        assert_eq!(dfg.value_type(new1), types::I64);
        assert_eq!(dfg.ebb_args(ebb), &[new1]);

        dfg.attach_ebb_arg(ebb, arg1);
        assert_eq!(dfg.ebb_args(ebb), &[new1, arg1]);

        let new2 = dfg.replace_ebb_arg(arg1, types::I8);
        assert_eq!(dfg.value_type(arg1), types::F32);
        assert_eq!(dfg.value_type(new2), types::I8);
        assert_eq!(dfg.ebb_args(ebb), &[new1, new2]);

        dfg.attach_ebb_arg(ebb, arg1);
        assert_eq!(dfg.ebb_args(ebb), &[new1, new2, arg1]);

        let new3 = dfg.replace_ebb_arg(new2, types::I16);
        assert_eq!(dfg.value_type(new1), types::I64);
        assert_eq!(dfg.value_type(new2), types::I8);
        assert_eq!(dfg.value_type(new3), types::I16);
        assert_eq!(dfg.ebb_args(ebb), &[new1, new3, arg1]);
    }

    #[test]
    fn aliases() {
        use ir::InstBuilder;
        use ir::entities::ExpandedValue::Direct;
        use ir::condcodes::IntCC;

        let mut func = Function::new();
        let dfg = &mut func.dfg;
        let ebb0 = dfg.make_ebb();
        let pos = &mut Cursor::new(&mut func.layout);
        pos.insert_ebb(ebb0);

        // Build a little test program.
        let v1 = dfg.ins(pos).iconst(types::I32, 42);

        // Make sure we can resolve value aliases even when extended_values is empty.
        assert_eq!(dfg.resolve_aliases(v1), v1);

        let arg0 = dfg.append_ebb_arg(ebb0, types::I32);
        let (s, c) = dfg.ins(pos).iadd_cout(v1, arg0);
        let iadd = match s.expand() {
            Direct(i) => i,
            _ => panic!(),
        };

        // Redefine the first value out of `iadd_cout`.
        assert_eq!(pos.prev_inst(), Some(iadd));
        let new_iadd = dfg.redefine_first_value(pos);
        let new_s = dfg.first_result(new_iadd);
        assert_eq!(dfg[iadd].opcode(), Opcode::Copy);
        assert_eq!(dfg.inst_results(iadd), &[s]);
        assert_eq!(dfg.inst_results(new_iadd), &[new_s, c]);
        assert_eq!(dfg.resolve_copies(s), new_s);
        pos.next_inst();

        // Detach the 'c' value from `iadd`.
        {
            assert_eq!(dfg.detach_secondary_results(new_iadd), Some(c));
            assert_eq!(dfg.next_secondary_result(c), None);
        }

        // Replace `iadd_cout` with a normal `iadd` and an `icmp`.
        dfg.replace(iadd).iadd(v1, arg0);
        let c2 = dfg.ins(pos).icmp(IntCC::UnsignedLessThan, s, v1);
        dfg.change_to_alias(c, c2);

        assert_eq!(dfg.resolve_aliases(c2), c2);
        assert_eq!(dfg.resolve_aliases(c), c2);

        // Make a copy of the alias.
        let c3 = dfg.ins(pos).copy(c);
        // This does not see through copies.
        assert_eq!(dfg.resolve_aliases(c3), c3);
        // But this goes through both copies and aliases.
        assert_eq!(dfg.resolve_copies(c3), c2);
    }
}
