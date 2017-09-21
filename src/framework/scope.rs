use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tf::TensorType;

use super::super::{DataType, Graph, OperationData, Output, Shape, Status, TypedTensor};
use super::IntoShape;
use ops::*;

/// Master context manager for building TensorFlow graphs and managing session execution.
#[derive(Debug)]
pub struct Scope {
    /// registry of tensor ops
    pub(crate) registry: Rc<RefCell<HashMap<Ident, TensorData>>>,
    /// owned graph for building
    pub(crate) graph: Rc<RefCell<Graph>>,
    pub(crate) scopes: Rc<RefCell<InternScope>>,
    pub(crate) own_scope: InternScope,
    pub(crate) control_context: ControlFlow,
    pub(crate) ops: Rc<RefCell<HashMap<Ident, OperationData>>>,
    reuse_variable: bool,
    not_variable_scope: bool,
    ignore_deps: bool,
    locked: Rc<RefCell<bool>>,
    parent_lock: Option<Rc<RefCell<bool>>>,
}

impl Scope {
    pub fn new() -> Scope {
        let own_scope = InternScope::new(PathBuf::new());
        Scope {
            scopes: Rc::new(RefCell::new(own_scope.clone())),
            own_scope,
            registry: Rc::new(RefCell::new(HashMap::new())),
            graph: Rc::new(RefCell::new(Graph::new())),
            control_context: ControlFlow::None,
            reuse_variable: false,
            not_variable_scope: false,
            ignore_deps: false,
            locked: Rc::new(RefCell::new(false)),
            parent_lock: None,
            ops: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// If `reuse`, don't return error on name collision with an existing identifier.
    pub(crate) fn resolve_tensor_name(
        &self,
        name: Option<&Path>,
        kind: IdType,
        reuse: bool,
    ) -> Result<PathBuf, ::Error> {
        let name = if let Some(given_name) = name {
            if given_name.to_str().unwrap() == "" {
                return self.resolve_tensor_name(None, kind, reuse);
            }
            let mut name = self.own_scope.name.join(given_name);
            if !reuse {
                match kind {
                    // check both constant and var containers in case a 'variable' was named
                    // like a constant and viceversa
                    IdType::Constant | IdType::Variable => {
                        if self.own_scope.name_exists(&name) {
                            return Err(::Error::Stub);
                        }
                    }
                    IdType::Placeholder => {
                        if self.registry
                            .borrow()
                            .values()
                            .find(|x| &x.full_name == &name)
                            .is_some()
                        {
                            return Err(::Error::Stub);
                        }
                    }
                    IdType::Operation(_) => {
                        if self.own_scope.ops.iter().find(|&&(ref x, _)| x == &name).is_some() {
                            name = self.own_scope.name.join(format!(
                                "{}_{}",
                                given_name.display(),
                                self.own_scope.ops.len()
                            ))
                        }
                    }
                }
            }
            name
        } else {
            let name = match kind {
                IdType::Constant => format!("Constant_{}", self.own_scope.constants.len()),
                IdType::Operation(name) => format!("{}_{}", name, self.own_scope.ops.len()),
                IdType::Placeholder => format!("Placeholder_{}", self.registry.borrow().len()),
                IdType::Variable => format!("Variable_{}", self.own_scope.variables.len()),
            };
            self.own_scope.name.join(name)
        };
        Ok(name)
    }

    pub(crate) fn resolve_new_scope_name<S: AsRef<Path>>(
        &self,
        name: S,
        default_prefix: &str,
    ) -> PathBuf {
        let new_name;
        if name_cmp!(name, "") {
            new_name = self.own_scope.name.join(format!(
                "{}_{}",
                default_prefix,
                self.own_scope.inner_scopes.len()
            ));
            new_name
        } else {
            self.own_scope.name.join(format!(
                "{}_{}",
                name.as_ref().display(),
                self.own_scope.inner_scopes.len()
            ))
        }
    }

    pub(crate) fn as_new_child(&mut self, name: PathBuf) -> Scope {
        // FIXME: use is_op to avoid duplicates
        let own_scope = if name == self.own_scope.name {
            // it's just a mirror of self, clone all the scope data,
            // writes on self will be locked, but after drop of the new scope,
            // they will be added from the mirror.
            self.own_scope.clone()
        } else {
            // find if this scope already exists
            let global = &mut self.scopes.borrow_mut().inner_scopes;
            let child = if let Some(scope) = find_parent_scope(global, &name, 0) {
                // an inmediate parent for this scope exists
                let mut add_new = false;
                let child;
                {
                    // initialize the new scope:
                    if let Some(found_child) = scope.get_child(&name) {
                        // this scope already existed, just return a copy of it
                        child = found_child.clone();
                    } else {
                        // this scope didn't exist previously, create it
                        child = InternScope::new(name.clone());
                        add_new = true;
                    };
                }
                if add_new {
                    // it was new, so add it to it's parent
                    scope.add_scope(child.clone());
                }
                Some(child)
            } else {
                // there was no inmediate parent for this scope
                None
            };
            if let Some(child) = child {
                child
            } else {
                // there was no inmediate parent for this scope, we are at root
                // create a new scope, add it to to 'scopes' and add it to 'self'
                let child = InternScope::new(name);
                global.push(Box::new(child.clone()));
                self.own_scope.add_scope(child.clone());
                child
            }
        };
        // lock self, new writes are only allowed on the returned subscope
        // if pertinent, when the new subscope is dropped, changes will be pulled
        // back to 'self' and the lock will be dropped.
        *self.locked.borrow_mut() = true;

        Scope {
            scopes: self.scopes.clone(),
            own_scope,
            registry: self.registry.clone(),
            graph: self.graph.clone(),
            control_context: self.control_context.clone(),
            reuse_variable: false,
            not_variable_scope: false,
            ignore_deps: false,
            locked: Rc::new(RefCell::new(false)),
            parent_lock: Some(self.locked.clone()),
            ops: self.ops.clone(),
        }
    }

    /// If this scope is locked, don't allow any write operations and panic at runtime.
    pub(crate) fn allow_writes(&self) {
        if *self.locked.borrow() {
            let scope_name = format!("{}", self.own_scope.name.display());
            let scope_name = match scope_name.as_str() {
                "" => format!("{}", "root"),
                _ => scope_name, 
            };
            static MSG_0: &str = "tried to write at scope `";
            static MSG_1: &str = "` while an other child scope was open";
            panic!(format!("{}{}{}", MSG_0, scope_name, MSG_1))
        }
    }

    pub(crate) fn locked(&self) -> bool {
        self.parent_lock.is_some()
    }

    /// Install an operation within the current context.
    ///
    /// Returns the output of the operations.
    #[doc(hidden)]
    pub fn install<'a, T>(&mut self, op: T) -> Result<T::Outputs, ::Error>
    where
        T: Operation<'a>,
    {
        self.allow_writes();
        let processed_inputs = self.process_op_inputs(&op);
        let new_op = {
            let graph = &mut *self.graph.borrow_mut();
            let root = &*self.scopes.borrow();

            let op_name = IdType::Operation(op.get_op_type_name());
            let name = if let Some(name) = op.get_op_name() {
                self.resolve_tensor_name(Some(name), op_name, false)?
            } else {
                self.resolve_tensor_name(None, op_name, false)?
            };
            let mut new_op = graph.new_operation(op.get_op_type_name(), name.to_str().unwrap())?;
            for input in processed_inputs {
                match input {
                    OpInput::Single(val) => new_op.add_input(val),
                    OpInput::List(val) => new_op.add_input_list(&val),
                }
            }
            let control_inputs = root.control_dependencies.iter().map(|x| &x.finished);
            add_control_input(&mut new_op, control_inputs);

            for &(name, is_list, ref attribute) in op.fetch_attributes() {
                match *attribute {
                    Attribute::String(ref val) => {
                        if val.len() > 1 || is_list {
                            new_op.set_attr_string_list(name, val)?;
                        } else if !val.is_empty() {
                            new_op.set_attr_string(name, val[0])?;
                        }
                    }
                    Attribute::Int(ref val) => {
                        if val.len() > 1 || is_list {
                            new_op.set_attr_int_list(name, val)?;
                        } else if !val.is_empty() {
                            new_op.set_attr_int(name, val[0])?;
                        }
                    }
                    Attribute::Float(ref val) => {
                        if val.len() > 1 || is_list {
                            new_op.set_attr_float_list(name, val)?;
                        } else if !val.is_empty() {
                            new_op.set_attr_float(name, val[0])?;
                        }
                    }
                    Attribute::Bool(ref val) => {
                        if val.len() > 1 || is_list {
                            new_op.set_attr_bool_list(name, val)?;
                        } else if !val.is_empty() {
                            new_op.set_attr_bool(name, val[0])?;
                        }
                    }
                    Attribute::Type(ref val) => {
                        if val.len() > 1 || is_list {
                            new_op.set_attr_type_list(name, val)?;
                        } else if !val.is_empty() {
                            new_op.set_attr_type(name, val[0])?;
                        }
                    }
                    Attribute::Shape(ref val) => {
                        if val.len() > 1 || is_list {
                            new_op.set_attr_shape_list(name, val)?;
                        } else if !val.is_empty() {
                            new_op.set_attr_shape(name, &val[0])?;
                        }
                    }
                    Attribute::Tensor(ref val) => {
                        if val.len() > 1 || is_list {
                            TensorContent::set_tensor_list_attr(&mut new_op, name, val)?;
                        } else if !val.is_empty() {
                            TensorContent::set_tensor_attr(&mut new_op, name, val)?;
                        }
                    }
                }
            }
            new_op.finish()?
        };
        op.digest(self, new_op)
    }

    #[allow(dead_code)]
    pub(crate) fn get_src_op<Op: Into<Ident>>(&self, op: Op) -> (OperationData, i32) {
        let &TensorData { data_origin: (ref op, idx), .. } = &self.registry.borrow()[&op.into()];
        (op.clone(), idx)
    }

    fn process_op_inputs<'a, T>(&mut self, op: &T) -> Vec<OpInput>
    where
        T: Operation<'a>,
    {
        let reg_c = self.registry.clone();
        let mut inputs = vec![];

        fn input_ls<'a>(
            input_lists: &mut ::std::slice::Iter<'a, (usize, Vec<Tensor>)>,
        ) -> (Option<usize>, Option<&'a [Tensor]>) {
            if let Some(&(ref idx, ref list)) = input_lists.next() {
                (Some(*idx), Some(list))
            } else {
                (None, None)
            }
        }

        fn iter_input_ls<'a>(
            input_lists: &mut ::std::slice::Iter<'a, (usize, Vec<Tensor>)>,
            reg: &HashMap<Ident, TensorData>,
            all_inputs: &mut Vec<OpInput>,
            args_index: &mut usize,
            ls_idx: &mut Option<usize>,
            current_list: &mut Option<&'a [Tensor]>,
        ) {
            while ls_idx.is_some() && (ls_idx.as_ref().unwrap() == args_index) {
                let mut inputs = vec![];
                for tensor in current_list.unwrap() {
                    let data = &reg[&tensor.ident];
                    inputs.push(Output {
                        operation: data.data_origin.0.clone(),
                        index: data.data_origin.1,
                    })
                }
                all_inputs.push(OpInput::List(inputs));
                let (i, ls) = input_ls(input_lists);
                *ls_idx = i;
                *current_list = ls;
                *args_index += 1;
            }
        }

        let mut args_index = 0_usize;
        let input_lists = &mut op.fetch_input_lists().iter();
        let (mut ls_idx, mut current_list) = input_ls(input_lists);
        for input in op.fetch_inputs().into_iter() {
            {
                iter_input_ls(
                    input_lists,
                    &*reg_c.borrow(),
                    &mut inputs,
                    &mut args_index,
                    &mut ls_idx,
                    &mut current_list,
                )
            }
            let (operation, index) = match self.control_context {
                ControlFlow::CondContext(_) => {
                    if !self.control_context.get_cond().unwrap().new_switch {
                        use self::control_flow_ops::CondContextInterface;
                        let output = self.process_output_tensor(input);
                        let data = &reg_c.borrow()[&output.ident];
                        (data.data_origin.0.clone(), data.data_origin.1)
                    } else {
                        let data = &reg_c.borrow()[&input.ident];
                        (data.data_origin.0.clone(), data.data_origin.1)
                    }
                }
                ControlFlow::None |
                ControlFlow::WhileContext(_) => {
                    let data = &reg_c.borrow()[&input.ident];
                    (data.data_origin.0.clone(), data.data_origin.1)
                }
            };
            inputs.push(OpInput::Single(Output { operation, index }));
            args_index += 1;
        }
        if current_list.is_some() {
            iter_input_ls(
                input_lists,
                &*reg_c.borrow(),
                &mut inputs,
                &mut args_index,
                &mut ls_idx,
                &mut current_list,
            );
        }
        inputs
    }

    /// Returns a context manager for defining ops that creates variables (layers).
    ///
    /// If the name argument is an empty string the name will be autogenerated.
    pub fn variable_scope<S>(
        &mut self,
        name: S,
        default_name: Option<S>,
        reuse: Option<bool>,
    ) -> Result<Scope, ::Error>
    where
        S: AsRef<Path>,
    {
        self.allow_writes();
        if name_cmp!(name, "") && default_name.is_none() {
            return Err(::Error::Msg(
                "If default_name is None then name is required not be empty."
                    .to_string(),
            ));
        }
        let name = if let Some(default_name) = default_name {
            self.resolve_new_scope_name(name, default_name.as_ref().to_str().unwrap())
        } else {
            self.resolve_new_scope_name(name, "")
        };
        let mut scope = self.as_new_child(name);
        if let Some(value) = reuse {
            scope.reuse_variable = value;
        }
        Ok(scope)
    }

    /// Returns a context manager for use when defining an op.
    ///
    /// This context manager validates that the given values are from the same graph and pushes a name scope in that graph.
    /// If the name argument is an empty string the name will be autogenerated.
    pub fn name_scope<S>(&mut self, name: S) -> Scope
    where
        S: AsRef<Path>,
    {
        self.allow_writes();
        let name = self.resolve_new_scope_name(name, "op_scope");
        let mut scope = self.as_new_child(name);
        scope.not_variable_scope = true;
        scope
    }

    /// Returns an error when creating a new variable and shape is not declared or when violating
    /// reuse during variable creation.
    ///
    /// Reuse is set during `variable_scope` creation.
    pub fn get_variable<IS, S>(
        &mut self,
        name: S,
        dtype: Option<DataType>,
        shape: Option<IS>,
    ) -> Result<Variable, ::Error>
    where
        S: AsRef<Path>,
        IS: IntoShape,
    {
        fn get_initial_value(
            g: &mut Graph,
            dtype: DataType,
            n: &str,
            shape: &[u64],
        ) -> Result<OperationData, Status> {
            match dtype {
                DataType::Bool => array_ops::constant(g, n, TypedTensor::<bool>::new(shape), &[]),
                DataType::Double => array_ops::constant(g, n, TypedTensor::<f64>::new(shape), &[]),
                DataType::Float => array_ops::constant(g, n, TypedTensor::<f32>::new(shape), &[]),
                DataType::Int32 => array_ops::constant(g, n, TypedTensor::<i32>::new(shape), &[]),
                DataType::UInt8 => array_ops::constant(g, n, TypedTensor::<u8>::new(shape), &[]),
                DataType::Int16 => array_ops::constant(g, n, TypedTensor::<i16>::new(shape), &[]),
                DataType::Int8 => array_ops::constant(g, n, TypedTensor::<i8>::new(shape), &[]),
                DataType::Int64 => array_ops::constant(g, n, TypedTensor::<i64>::new(shape), &[]),
                _ => unimplemented!(),
            }
        }

        self.allow_writes();
        let new_var = self.resolve_tensor_name(Some(name.as_ref()), IdType::Variable, false)?;

        let var = if self.not_variable_scope {
            // use scopes/root
            // FIXME: don't default to root
            self.scopes.borrow().variables.binary_search_by(|&(ref name, _)| name.cmp(&new_var))
        } else {
            (&self.own_scope.variables).binary_search_by(|&(ref name, _)| name.cmp(&new_var))
        };
        if self.reuse_variable {
            // find and return an existing variable or return error
            if let Ok(idx) = var {
                Ok(self.own_scope.variables[idx].1)
            } else {
                Err(::Error::Stub)
            }
        } else if var.is_err() && !self.reuse_variable {
            // try making a new variable
            let rank_info = if let Some(shape) = shape {
                shape.into_shape()
            } else {
                // shape for a new variable must be specified
                return Err(::Error::Stub);
            };
            let dtype = if let Some(dtype) = dtype {
                dtype
            } else {
                return Err(::Error::Msg("dtype not specified".to_owned()));
            };

            let ident = Ident::new();
            let init;
            let var;
            {
                let graph = &mut *self.graph.borrow_mut();
                let registry = &*self.registry.borrow();

                // variable op, not initialized!
                var = {
                    let deps = match self.control_context {
                        ControlFlow::CondContext(ref cond) => {
                            vec![&registry[&cond.pivot.ident].data_origin.0]
                        }
                        ControlFlow::WhileContext(ref cond) => {
                            if cond.pivot_for_body.is_some() {
                                vec![
                                    &registry[&cond.pivot_for_body.as_ref().unwrap().ident]
                                        .data_origin
                                        .0,
                                ]
                            } else {
                                vec![
                                    &registry[&cond.pivot_for_pred.as_ref().unwrap().ident]
                                        .data_origin
                                        .0,
                                ]
                            }
                        }
                        ControlFlow::None => vec![],
                    };
                    init_ops::variable_(graph, new_var.to_str().unwrap(), dtype, &rank_info, deps)?
                };

                // initializer
                init = {
                    let initial_value = {
                        let init_name = new_var.join("init_value");
                        get_initial_value(
                            graph,
                            dtype,
                            init_name.to_str().unwrap(),
                            &rank_info.definition_u64().unwrap(),
                        )?
                    };
                    let init = &[
                        init_ops::assign_(
                            graph,
                            new_var.join("init").to_str().unwrap(),
                            var.clone(),
                            (initial_value, 0),
                            false,
                        )?,
                    ];
                    // get previous existing control dependencies
                    let cd = &self.scopes.borrow().control_dependencies;
                    let control_inputs = cd.iter().map(|x| &x.finished).chain(init);
                    control_flow_ops::no_op_(
                        graph,
                        new_var.join("init_ctrl").to_str().unwrap(),
                        control_inputs,
                    )?
                };
            }
            {
                let registry = &mut *self.registry.borrow_mut();
                registry.insert(
                    ident,
                    TensorData {
                        full_name: new_var.clone(),
                        idtype: IdType::Variable,
                        dtype,
                        data_origin: (var, 0),
                        shape: rank_info,
                    },
                );
                self.scopes.borrow_mut().control_dependencies.push_front(ControlOp {
                    ident: Ident::new(),
                    finished: init,
                    kind: ControlOpKind::VarInitializer,
                });
            }
            Ok(self._make_var_handle(ident, new_var, dtype))
        } else {
            Err(::Error::Stub)
        }
    }

    pub fn get_variable_with_initializer<S, T>(
        &mut self,
        name: S,
        initializer: T,
        validate_shape: bool,
    ) -> Result<Variable, ::Error>
    where
        S: AsRef<Path>,
        T: Into<Ident>,
    {
        self.allow_writes();
        let new_var = self.resolve_tensor_name(Some(name.as_ref()), IdType::Variable, false)?;

        let var = if self.not_variable_scope {
            // use scopes/root
            // FIXME: don't default to root
            self.scopes.borrow().variables.binary_search_by(|&(ref name, _)| name.cmp(&new_var))
        } else {
            (&self.own_scope.variables).binary_search_by(|&(ref name, _)| name.cmp(&new_var))
        };

        if self.reuse_variable {
            // find and return an existing variable or return error
            if let Ok(idx) = var {
                Ok(self.own_scope.variables[idx].1)
            } else {
                Err(::Error::Stub)
            }
        } else if var.is_err() && !self.reuse_variable {
            let ident = Ident::new();
            let init;
            let var;
            let dtype;
            let rank_info;
            {
                let graph = &mut *self.graph.borrow_mut();
                let registry = &mut *self.registry.borrow_mut();

                let initializer = registry.get(&initializer.into()).unwrap();
                rank_info = graph.tensor_shape(Output {
                    operation: initializer.data_origin.0.clone(),
                    index: initializer.data_origin.1,
                })?;
                dtype = initializer.dtype;

                // variable op, not initialized!
                var = {
                    let deps = match self.control_context {
                        ControlFlow::CondContext(ref cond) => {
                            vec![&registry[&cond.pivot.ident].data_origin.0]
                        }
                        ControlFlow::WhileContext(ref cond) => {
                            if cond.pivot_for_body.is_some() {
                                vec![
                                    &registry[&cond.pivot_for_body.as_ref().unwrap().ident]
                                        .data_origin
                                        .0,
                                ]
                            } else {
                                vec![
                                    &registry[&cond.pivot_for_pred.as_ref().unwrap().ident]
                                        .data_origin
                                        .0,
                                ]
                            }
                        }
                        ControlFlow::None => vec![],
                    };
                    init_ops::variable_(graph, new_var.to_str().unwrap(), dtype, &rank_info, deps)?
                };

                // initializer
                init = {
                    let init = &[
                        init_ops::assign_(
                            graph,
                            new_var.join("init").to_str().unwrap(),
                            var.clone(),
                            initializer.data_origin.clone(),
                            validate_shape,
                        )?,
                    ];

                    // get previous existing control dependencies
                    let cd = &self.scopes.borrow().control_dependencies;
                    let control_inputs = cd.iter().map(|x| &x.finished).chain(init);
                    control_flow_ops::no_op_(
                        graph,
                        new_var.join("init_ctrl").to_str().unwrap(),
                        control_inputs,
                    )?
                };
            }
            {
                let registry = &mut *self.registry.borrow_mut();
                registry.insert(
                    ident,
                    TensorData {
                        full_name: new_var.clone(),
                        idtype: IdType::Variable,
                        dtype,
                        data_origin: (var, 0),
                        shape: rank_info,
                    },
                );
                self.scopes.borrow_mut().control_dependencies.push_front(ControlOp {
                    ident: Ident::new(),
                    finished: init,
                    kind: ControlOpKind::VarInitializer,
                });
            }
            Ok(self._make_var_handle(ident, new_var, dtype))
        } else {
            Err(::Error::Stub)
        }
    }

    fn _make_var_handle(&mut self, ident: Ident, new_var: PathBuf, dtype: DataType) -> Variable {
        // make handle
        let var = Variable {
            ident,
            dtype,
            initializer: Ident::new(),
            idx: 0,
        };
        if !self.not_variable_scope {
            // use local
            self.own_scope.variables.push((new_var, var));
            var
        } else {
            // use scopes
            self.scopes.borrow_mut().variables.push((new_var, var));
            var
        }
    }

    pub fn constant<TeS, T, S>(
        &mut self,
        name: S,
        value: &[T],
        shape: &[TeS],
    ) -> Result<Constant, ::Error>
    where
        S: AsRef<Path>,
        T: TensorType,
        TeS: ShapeSize,
    {
        self.allow_writes();
        let graph = &mut *self.graph.borrow_mut();
        let registry = &mut *self.registry.borrow_mut();

        let full_name = self.resolve_tensor_name(Some(name.as_ref()), IdType::Constant, false)?;
        let ident = Ident::new();

        let shape: &[u64] = &shape_as_u64(shape);

        let data_origin = {
            let cd = &self.scopes.borrow().control_dependencies;
            match self.control_context {
                ControlFlow::CondContext(ref cond) => {
                    let pivot = vec![&registry[&cond.pivot.ident].data_origin.0];
                    array_ops::constant(
                        graph,
                        full_name.to_str().unwrap(),
                        to_tf_tensor![value; shape],
                        cd.iter().map(|x| &x.finished).chain(pivot),
                    )?
                }
                ControlFlow::WhileContext(ref cond) => {
                    let pivot = if cond.pivot_for_body.is_some() {
                        vec![
                            &registry[&cond.pivot_for_body.as_ref().unwrap().ident].data_origin.0,
                        ]
                    } else {
                        vec![
                            &registry[&cond.pivot_for_pred.as_ref().unwrap().ident].data_origin.0,
                        ]
                    };
                    array_ops::constant(
                        graph,
                        full_name.to_str().unwrap(),
                        to_tf_tensor![value; shape],
                        cd.iter().map(|x| &x.finished).chain(pivot),
                    )?
                }
                ControlFlow::None => {
                    array_ops::constant(
                        graph,
                        full_name.to_str().unwrap(),
                        to_tf_tensor![value; shape],
                        cd.iter().map(|x| &x.finished),
                    )?
                }
            }
        };
        let dtype = data_origin.output_type(0);
        registry.insert(
            ident,
            TensorData {
                full_name: full_name.clone(),
                dtype: dtype,
                idtype: IdType::Constant,
                data_origin: (data_origin.clone(), 0),
                shape: graph.tensor_shape(Output {
                    operation: data_origin,
                    index: 0,
                })?,
            },
        );

        self.own_scope.constants.push((full_name, ident));
        Ok(Constant { ident, dtype })
    }

    pub fn placeholder(&mut self, dtype: DataType) -> Tensor {
        self.allow_writes();
        let graph = &mut *self.graph.borrow_mut();
        let registry = &mut *self.registry.borrow_mut();

        let ident = Ident::new();
        let full_name = self.resolve_tensor_name(None, IdType::Constant, false).unwrap();
        let data_origin = (
            array_ops::placeholder(graph, full_name.to_str().unwrap(), dtype)
                .unwrap(),
            0,
        );
        registry.insert(
            ident,
            TensorData {
                full_name,
                dtype,
                idtype: IdType::Placeholder,
                data_origin,
                shape: Shape::from(None),
            },
        );

        Tensor {
            ident,
            idtype: IdType::Placeholder,
            dtype,
            idx: 0,
        }
    }

    pub fn control_dependencies<'a, I, T: 'a>(&mut self, control_inputs: I) -> Scope
    where
        I: IntoIterator<Item = &'a T>,
        T: GetIdent,
    {
        self.allow_writes();
        let name = self.own_scope.name.clone();
        let mut context = self.as_new_child(name);
        let op_name = self.resolve_tensor_name(None, IdType::Operation("NoOp"), false).unwrap();

        let registry = &*self.registry.borrow();
        let existing_ops = &*self.ops.borrow();
        let global = &mut self.scopes.borrow_mut().control_dependencies;

        let mut ops = vec![];
        for control_input in control_inputs.into_iter() {
            let ident = control_input.get_ident();
            let ctrl = if let Some(op) = existing_ops.get(&ident) {
                ops.push(op);
                ControlOp {
                    ident,
                    finished: op.clone(),
                    kind: ControlOpKind::Ops,
                }
            } else {
                let finished = &registry[&ident].data_origin.0;
                ops.push(finished);
                ControlOp {
                    ident,
                    finished: finished.clone(),
                    kind: ControlOpKind::Other,
                }
            };
            context.own_scope.control_dependencies.push_back(ctrl.clone());
            global.push_back(ctrl);
        }

        let ctrl_group = {
            let graph = &mut *self.graph.borrow_mut();
            ControlOp {
                ident: Ident::new(),
                finished: control_flow_ops::no_op_(graph, op_name.to_str().unwrap(), ops).unwrap(),
                kind: ControlOpKind::Ops,
            }
        };
        global.push_front(ctrl_group);
        context
    }

    pub fn clear_control_dependencies(&mut self) -> Scope {
        let name = self.own_scope.name.clone();
        let mut context = self.as_new_child(name);
        context.ignore_deps = true;
        context
    }

    /// Returns a copy of the variable, with the same shape and content.
    pub fn identity<S>(&mut self, name: S, tensor: Tensor) -> Result<Tensor, ::Error>
    where
        S: AsRef<Path>,
    {
        self.allow_writes();
        let full_name = self.resolve_tensor_name(Some(name.as_ref()), tensor.idtype, false)?;

        let graph = &mut *self.graph.borrow_mut();
        let registry = &mut *self.registry.borrow_mut();
        let global = &self.scopes.borrow().control_dependencies;

        let src = registry[&tensor.ident].data_origin.clone();
        let ident = Ident::new();
        let data_origin = (
            array_ops::identity(
                graph,
                full_name.to_str().unwrap(),
                src,
                global.iter().map(|x| &x.finished),
            )?,
            0,
        );
        registry.insert(
            ident,
            TensorData {
                full_name,
                dtype: tensor.dtype,
                idtype: IdType::Operation("Identity"),
                data_origin,
                shape: Shape::from(None),
            },
        );

        Ok(Tensor {
            ident,
            dtype: tensor.dtype,
            idtype: tensor.idtype,
            idx: 0,
        })
    }

    /// Can only be set at root scope context.
    pub fn set_random_seed(&mut self, value: Option<i32>) {
        if self.parent_lock.is_some() {
            panic!("random seed can only be set at the root scope");
        }
        unimplemented!()
    }

    pub fn get_seed(&self, op_seed: Option<i32>) -> (i32, i32) {
        unimplemented!()
    }

    /// Marks the given op/tensor as unfetchable in this graph.
    pub fn prevent_fetching<Op: Into<Ident>>(&mut self, op: Op) {
        self.scopes.borrow_mut().unfetchable.insert(op.into());
    }

    /// Marks the given op/tensor as unfetchable in this graph.
    pub fn prevent_feeding<Op: Into<Ident>>(&mut self, op: Op) {
        unimplemented!()
    }

    /// Consumes self and returns underlying graph if it's a unique reference, otherwise
    /// will return a Rc pointer to it.
    pub fn unwrap_graph(mut self) -> Result<Graph, Rc<RefCell<Graph>>> {
        let mut graph = Rc::new(RefCell::new(Graph::new()));
        ::std::mem::swap(&mut graph, &mut self.graph);
        match Rc::try_unwrap(graph) {
            Ok(cell) => Ok(cell.into_inner()),
            Err(rc) => Err(rc),
        }
    }

    pub fn name(&self) -> &str {
        self.own_scope.name.to_str().unwrap()
    }
}

impl ::std::ops::Drop for Scope {
    fn drop(&mut self) {
        use std::mem::swap;
        let mut new_scope = InternScope::new(PathBuf::new());
        swap(&mut new_scope, &mut self.own_scope);
        let global = &mut *self.scopes.borrow_mut();
        let InternScope {
            name,
            variables,
            constants,
            ops,
            inner_scopes,
            control_dependencies,
            unfetchable,
        } = new_scope;
        // find if this scope already exists
        if let Some(parent) = find_parent_scope(&mut global.inner_scopes, &name, 0) {
            // merge changes into it
            parent.inner_scopes.extend(inner_scopes);
            parent.variables.extend(variables);
            parent.ops.extend(ops);
            parent.constants.extend(constants);
        }
        // pop dependencies in this scope, if there are any
        let original_deps = global.control_dependencies.len() - control_dependencies.len();
        global.control_dependencies.truncate(original_deps);
        // add to global scope:
        global.unfetchable.extend(unfetchable);
        if let Some(lock) = self.parent_lock.as_ref() {
            *lock.borrow_mut() = false;
        }
    }
}


enum OpInput {
    List(Vec<Output>),
    Single(Output),
}

#[derive(Debug, Clone)]
pub(crate) struct InternScope {
    /// Full path for this scope.
    pub(crate) name: PathBuf,
    /// Variables only available in this scope.
    variables: Vec<(PathBuf, Variable)>,
    /// Constants only available in this scope.
    constants: Vec<(PathBuf, Ident)>,
    /// Ops declared in this scope.
    pub(crate) ops: Vec<(PathBuf, Ident)>,
    /// Children scopes.
    inner_scopes: Vec<Box<InternScope>>,
    /// Control dependencies in this scope
    pub(crate) control_dependencies: VecDeque<ControlOp>,
    /// Unfetchable tensors.
    unfetchable: HashSet<Ident>,
}

impl InternScope {
    fn new(name: PathBuf) -> InternScope {
        InternScope {
            name,
            variables: vec![],
            constants: vec![],
            inner_scopes: vec![],
            ops: vec![],
            control_dependencies: VecDeque::new(),
            unfetchable: HashSet::new(),
        }
    }

    fn add_scope(&mut self, scope: InternScope) {
        self.inner_scopes.push(Box::new(scope));
        self.inner_scopes.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    }

    fn get_child(&self, name: &Path) -> Option<&InternScope> {
        let f = name.file_name().unwrap();
        for scope in &self.inner_scopes {
            if let Ok(rest) = name.strip_prefix(&self.name) {
                if rest == f {
                    return Some(self);
                } else {
                    return scope.get_child(name);
                }
            }
        }
        None
    }

    fn name_exists(&self, name: &Path) -> bool {
        for &(ref var_name, _) in &self.variables {
            if var_name == name {
                return true;
            }
        }
        for &(ref const_name, _) in &self.constants {
            if const_name == name {
                return true;
            }
        }
        false
    }
}

fn find_parent_scope<'a>(
    scopes: &'a mut [Box<InternScope>],
    name: &Path,
    comp: usize,
) -> Option<&'a mut InternScope> {
    for scope in scopes {
        let rest = if let Some((_, prefix)) =
            scope.name.iter().enumerate().skip_while(|&(ref i, _)| i < &comp).find(|&(_, p)| {
                name.starts_with(p)
            })
        {
            name.strip_prefix(prefix).unwrap().to_owned()
        } else {
            continue;
        };
        if rest.parent().is_none() {
            return Some(scope);
        }
        return find_parent_scope(&mut scope.inner_scopes, &rest, comp + 1);
    }
    None
}

impl PartialEq for InternScope {
    fn eq(&self, rhs: &InternScope) -> bool {
        self.name == rhs.name
    }
}
