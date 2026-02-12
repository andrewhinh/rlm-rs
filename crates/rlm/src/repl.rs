use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rustpython_pylib;
use rustpython_stdlib;
use rustpython_vm as vm;
use rustpython_vm::builtins::{PyBaseException, PyDictRef};
use rustpython_vm::scope::Scope;
use rustpython_vm::{Interpreter, InterpreterBuilder};
use tempfile::TempDir;

use crate::llm::{LlmClient, Message};
use crate::utils::ContextData;

#[derive(Clone, Debug)]
pub struct LocalValue {
    pub name: String,
    pub repr: String,
    pub is_simple: bool,
    pub string_value: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ReplResult {
    pub stdout: String,
    pub stderr: String,
    pub locals: Vec<LocalValue>,
    pub locals_map: Vec<(String, String)>,
    pub execution_time: f64,
}

const EXECUTION_TIMEOUT_SECS: f64 = 10.0;

pub struct ReplEnv {
    interpreter: Interpreter,
    scope: Scope,
    temp_dir: TempDir,
    llm_client: Arc<dyn LlmClient>,
    execution_lock: Mutex<()>,
}

impl ReplEnv {
    pub fn new(
        context: ContextData,
        llm_client: Arc<dyn LlmClient>,
        setup_code: Option<&str>,
    ) -> anyhow::Result<Self> {
        let builder = InterpreterBuilder::new();
        let interpreter = init_stdlib(builder).interpreter();
        let scope = interpreter
            .enter(|vm: &vm::VirtualMachine| {
                let scope = vm.new_scope_with_builtins();
                Ok(scope)
            })
            .map_err(|err: vm::PyRef<PyBaseException>| {
                anyhow::anyhow!("python init error: {err:?}")
            })?;
        let temp_dir = TempDir::new()?;

        let mut env = Self {
            interpreter,
            scope,
            temp_dir,
            llm_client,
            execution_lock: Mutex::new(()),
        };
        env.initialize(context)?;
        if let Some(code) = setup_code {
            env.execute(code)?;
        }
        Ok(env)
    }

    fn initialize(&mut self, context: ContextData) -> anyhow::Result<()> {
        let llm_client = self.llm_client.clone();
        let scope = self.scope.clone();
        let temp_dir = self.temp_dir.path().to_path_buf();
        let temp_dir_str = temp_dir.to_string_lossy().to_string();
        let mut json_path: Option<String> = None;
        let mut text_path: Option<String> = None;

        if let Some(json_value) = context.json {
            let path = temp_dir.join("context.json");
            let payload = serde_json::to_vec_pretty(&json_value)?;
            fs::write(&path, payload)?;
            json_path = Some(path.to_string_lossy().to_string());
        }

        if let Some(text) = context.text {
            let path = temp_dir.join("context.txt");
            fs::write(&path, text)?;
            text_path = Some(path.to_string_lossy().to_string());
        }

        let enter_result = self
            .interpreter
            .enter(move |vm: &vm::VirtualMachine| -> vm::PyResult<()> {
            scope
                .globals
                .set_item(
                    "__rlm_temp_dir",
                    vm.ctx.new_str(temp_dir_str.as_str()).into(),
                    vm,
                )?;
            let llm_fn = vm.new_function(
                "__rlm_llm_query",
                move |prompt: String| -> vm::PyResult<String> {
                    let messages = parse_llm_prompt(&prompt);
                    let response = llm_client
                        .completion(&messages, None)
                        .unwrap_or_else(|err| format!("Error making LLM query: {err}"));
                    Ok(response)
                }
            );
            scope
                .globals
                .set_item("__rlm_llm_query", llm_fn.into(), vm)?;
            let init_segments = [
                (
                    "builtins_ref",
                    r#"__rlm_builtins = __builtins__
if isinstance(__rlm_builtins, dict):
    def __rlm_get_builtin(name):
        return __rlm_builtins.get(name)
else:
    def __rlm_get_builtin(name):
        return getattr(__rlm_builtins, name, None)
"#,
                ),
                (
                    "builtin_refs",
                    "__rlm_exec_builtin = __rlm_get_builtin('exec')\n__rlm_eval_builtin = __rlm_get_builtin('eval')\n__rlm_globals_builtin = __rlm_get_builtin('globals')\n",
                ),
                (
                    "safe_list",
                    r#"__rlm_safe_builtin_names = [
    "print", "len", "str", "int", "float", "list", "dict", "set", "tuple", "bool",
    "type", "isinstance", "enumerate", "zip", "map", "filter", "sorted", "min", "max",
    "sum", "abs", "round", "chr", "ord", "hex", "bin", "oct", "repr", "ascii", "format",
    "__import__", "open", "any", "all", "hasattr", "getattr", "setattr", "delattr", "dir",
    "vars", "range", "reversed", "slice", "iter", "next", "pow", "divmod", "complex",
    "bytes", "bytearray", "memoryview", "hash", "id", "callable", "issubclass", "super",
    "property", "staticmethod", "classmethod", "object", "BaseException", "ArithmeticError",
    "LookupError", "EnvironmentError", "AssertionError", "NotImplementedError", "UnicodeError",
    "Warning", "UserWarning", "DeprecationWarning", "PendingDeprecationWarning", "SyntaxWarning",
    "RuntimeWarning", "FutureWarning", "ImportWarning", "UnicodeWarning", "BytesWarning",
    "ResourceWarning", "Exception", "ValueError", "TypeError", "KeyError", "IndexError",
    "AttributeError", "FileNotFoundError", "OSError", "IOError", "RuntimeError", "NameError",
    "ImportError", "StopIteration", "GeneratorExit", "SystemExit", "KeyboardInterrupt",
]"#,
                ),
                (
                    "safe_builtins",
                    "__rlm_safe_builtins = {}\nfor __rlm_name in __rlm_safe_builtin_names:\n    __rlm_value = __rlm_get_builtin(__rlm_name)\n    if __rlm_value is not None:\n        __rlm_safe_builtins[__rlm_name] = __rlm_value\n",
                ),
                (
                    "safe_blocklist",
                    "for __rlm_name in [\"input\", \"eval\", \"exec\", \"compile\", \"globals\", \"locals\"]:\n    __rlm_safe_builtins[__rlm_name] = None\n",
                ),
                (
                    "safe_imports",
                    r#"__rlm_allowed_modules = {
    "json", "math", "statistics", "random", "re", "itertools", "functools",
    "collections", "datetime", "decimal", "fractions", "io", "sys", "time"
}
__rlm_import_builtin = __rlm_get_builtin('__import__')
def __rlm_safe_import(name, globals=None, locals=None, fromlist=(), level=0, _import=__rlm_import_builtin):
    root = name.split('.')[0]
    if root not in __rlm_allowed_modules:
        raise ImportError(f"Import of '{root}' is blocked")
    return _import(name, globals, locals, fromlist, level)
"#,
                ),
                (
                    "safe_open",
                    r#"__rlm_open_builtin = __rlm_get_builtin('open')
def __rlm_safe_open(path, *args, _import=__rlm_import_builtin, _open=__rlm_open_builtin, _root=__rlm_temp_dir, **kwargs):
    __rlm_os = _import('os')
    __rlm_root = __rlm_os.path.abspath(_root)
    __rlm_path = str(path)
    if not __rlm_os.path.isabs(__rlm_path):
        __rlm_path = __rlm_os.path.join(__rlm_root, __rlm_path)
    __rlm_path = __rlm_os.path.abspath(__rlm_path)
    if not (__rlm_path == __rlm_root or __rlm_path.startswith(__rlm_root + __rlm_os.sep)):
        raise PermissionError("open restricted to temp dir")
    return _open(__rlm_path, *args, **kwargs)
"#,
                ),
                (
                    "safe_cleanup",
                    "del __rlm_import_builtin\ndel __rlm_open_builtin\n",
                ),
                (
                    "safe_overrides",
                    "__rlm_safe_builtins['__import__'] = __rlm_safe_import\n__rlm_safe_builtins['open'] = __rlm_safe_open\n",
                ),
                ("builtins_assign", "__builtins__ = __rlm_safe_builtins\n"),
                ("locals_init", "__rlm_locals = {}\n"),
                (
                    "llm_query",
                    r#"def llm_query(prompts):
    __rlm_json = __rlm_get_builtin('__import__')('json')
    if isinstance(prompts, list):
        payload = __rlm_json.dumps(prompts, default=str)
    else:
        payload = __rlm_json.dumps([prompts], default=str)
    return __rlm_llm_query(payload)
"#,
                ),
                (
                    "final_var",
                    r#"def FINAL_VAR(name):
    name = name.strip().strip('"').strip("'").strip('\n').strip('\r')
    if name in __rlm_locals:
        return __rlm_locals[name]
    return f"Error: Variable '{name}' not found in REPL environment"
"#,
                ),
                (
                    "rlm_exec",
                    r#"def __rlm_exec(code):
    __rlm_globals = __rlm_globals_builtin()
    lines = code.split('\n')
    import_lines = []
    other_lines = []
    for line in lines:
        if line.startswith(('import ', 'from ')) and not line.startswith('#'):
            import_lines.append(line)
        else:
            other_lines.append(line)

    if import_lines:
        import_code = '\n'.join(import_lines)
        __rlm_exec_builtin(import_code, __rlm_globals, __rlm_globals)

    if other_lines:
        other_code = '\n'.join(other_lines)
        combined_namespace = {**__rlm_globals, **__rlm_locals}
        non_comment_lines = [line for line in other_lines if line and not line.startswith('#')]

        if non_comment_lines:
            last_line = non_comment_lines[-1]
            is_expression = (
                not last_line.startswith(('import ', 'from ', 'def ', 'class ', 'if ', 'for ', 'while ', 'try:', 'with ', 'return ', 'yield ', 'break', 'continue', 'pass')) and
                '=' not in last_line.split('#')[0] and
                not last_line.endswith(':') and
                not last_line.startswith('print(')
            )

            if is_expression:
                try:
                    if len(non_comment_lines) > 1:
                        last_line_start = -1
                        for i, line in enumerate(other_lines):
                            if line == last_line:
                                last_line_start = i
                                break
                        if last_line_start > 0:
                            statements_code = '\n'.join(other_lines[:last_line_start])
                            __rlm_exec_builtin(statements_code, combined_namespace, combined_namespace)

                    result = __rlm_eval_builtin(last_line, combined_namespace, combined_namespace)
                    if result is not None:
                        print(repr(result))
                except Exception:
                    __rlm_exec_builtin(other_code, combined_namespace, combined_namespace)
            else:
                __rlm_exec_builtin(other_code, combined_namespace, combined_namespace)
        else:
            __rlm_exec_builtin(other_code, combined_namespace, combined_namespace)

        for key, value in combined_namespace.items():
            if key not in __rlm_globals:
                __rlm_locals[key] = value
"#,
                ),
            ];

            for (label, code) in init_segments {
                vm.run_string(scope.clone(), code, format!("<rlm_init_{label}>"))?;
            }
            if let Some(ref path_str) = json_path {
                scope
                    .globals
                    .set_item(
                        "__rlm_context_json_path",
                        vm.ctx.new_str(path_str.as_str()).into(),
                        vm,
                    )?;
                let code =
                    "import json\nwith open(__rlm_context_json_path, \"r\") as f:\n    context = json.load(f)\n";
                vm.run_string(scope.clone(), code, "<rlm_context_json>".to_owned())?;
            }

            if let Some(ref path_str) = text_path {
                scope
                    .globals
                    .set_item(
                        "__rlm_context_text_path",
                        vm.ctx.new_str(path_str.as_str()).into(),
                        vm,
                    )?;
                let code = "with open(__rlm_context_text_path, \"r\") as f:\n    context = f.read()\n";
                vm.run_string(scope.clone(), code, "<rlm_context_text>".to_owned())?;
            }
            Ok(())
        });
        enter_result.map_err(|err: vm::PyRef<PyBaseException>| {
            anyhow::anyhow!("python init error: {err:?}")
        })?;

        Ok(())
    }

    pub fn execute(&mut self, code: &str) -> anyhow::Result<ReplResult> {
        let _lock = self
            .execution_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("repl lock poisoned"))?;
        let scope = self.scope.clone();
        let temp_dir = self.temp_dir.path().to_path_buf();
        let start = Instant::now();

        let mut result = self
            .interpreter
            .enter(|vm: &vm::VirtualMachine| -> vm::PyResult<ReplResult> {
            let temp_dir_str = temp_dir.to_string_lossy().to_string();
            scope.globals.set_item(
                "__rlm_temp_dir",
                vm.ctx.new_str(temp_dir_str.as_str()).into(),
                vm,
            )?;
            let preamble = format!(
                "import io, sys, time\n__rlm_old_stdout = sys.stdout\n__rlm_old_stderr = sys.stderr\n__rlm_stdout = io.StringIO()\n__rlm_stderr = io.StringIO()\nsys.stdout = __rlm_stdout\nsys.stderr = __rlm_stderr\n__rlm_exec_deadline = time.time() + {EXECUTION_TIMEOUT_SECS}\n\ndef __rlm_trace(frame, event, arg):\n    if time.time() > __rlm_exec_deadline:\n        raise TimeoutError('Execution time limit exceeded')\n    return __rlm_trace\n\nsys.settrace(__rlm_trace)\n"
            );
            vm.run_string(scope.clone(), &preamble, "<rlm_preamble>".to_owned())?;
            scope
                .globals
                .set_item("__rlm_code", vm.ctx.new_str(code).into(), vm)?;
            match vm.run_string(scope.clone(), "__rlm_exec(__rlm_code)\n", "<rlm_exec>".to_owned())
            {
                Ok(_) => {}
                Err(exc) => {
                    vm.print_exception(exc);
                }
            }

            let postamble = "import sys\nsys.settrace(None)\nsys.stdout = __rlm_old_stdout\nsys.stderr = __rlm_old_stderr\n__rlm_stdout_value = __rlm_stdout.getvalue()\n__rlm_stderr_value = __rlm_stderr.getvalue()\n__rlm_locals['_stdout'] = __rlm_stdout_value\n__rlm_locals['_stderr'] = __rlm_stderr_value\n";
            vm.run_string(scope.clone(), postamble, "<rlm_postamble>".to_owned())?;

            let stdout = get_string_from_scope(vm, &scope, "__rlm_stdout_value");
            let stderr = get_string_from_scope(vm, &scope, "__rlm_stderr_value");
            let locals = collect_locals(vm, &scope);
            let locals_map = collect_locals_map(vm, &scope);
            Ok(ReplResult {
                stdout,
                stderr,
                locals,
                locals_map,
                execution_time: start.elapsed().as_secs_f64(),
            })
        })
            .map_err(|err: vm::PyRef<PyBaseException>| {
                anyhow::anyhow!("python exec error: {err:?}")
            })?;

        result.execution_time = start.elapsed().as_secs_f64();
        Ok(result)
    }

    pub fn get_variable(&self, name: &str) -> anyhow::Result<Option<String>> {
        let scope = self.scope.clone();
        self.interpreter
            .enter(|vm: &vm::VirtualMachine| -> vm::PyResult<Option<String>> {
                let locals = get_locals_dict(vm, &scope);
                let value = locals.and_then(|dict| dict.get_item(name, vm).ok());
                if let Some(value) = value {
                    let text = match value.str(vm) {
                        Ok(py_str) => py_str.as_str().to_owned(),
                        Err(_) => value.repr(vm)?.as_str().to_owned(),
                    };
                    Ok(Some(text))
                } else {
                    Ok(None)
                }
            })
            .map_err(|err: vm::PyRef<PyBaseException>| {
                anyhow::anyhow!("python variable error: {err:?}")
            })
    }

    pub fn get_cost_summary(&self) -> anyhow::Result<()> {
        anyhow::bail!("Cost tracking is not implemented for the REPL Environment.")
    }
}

fn init_stdlib(builder: InterpreterBuilder) -> InterpreterBuilder {
    let defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
    builder
        .add_native_modules(&defs)
        .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
        .init_hook(set_frozen_stdlib_dir)
}

fn set_frozen_stdlib_dir(vm: &mut vm::VirtualMachine) {
    use rustpython_vm::common::rc::PyRc;

    let state = PyRc::get_mut(&mut vm.state).expect("vm state");
    state.config.paths.stdlib_dir = Some(rustpython_pylib::LIB_PATH.to_owned());
}

fn get_string_from_scope(vm: &vm::VirtualMachine, scope: &Scope, name: &str) -> String {
    scope
        .globals
        .get_item(name, vm)
        .ok()
        .and_then(|value| value.try_to_value::<String>(vm).ok())
        .unwrap_or_default()
}

fn get_locals_dict(vm: &vm::VirtualMachine, scope: &Scope) -> Option<PyDictRef> {
    scope
        .globals
        .get_item("__rlm_locals", vm)
        .ok()
        .and_then(|value| value.downcast::<vm::builtins::PyDict>().ok())
}

fn collect_locals(vm: &vm::VirtualMachine, scope: &Scope) -> Vec<LocalValue> {
    let dict = match get_locals_dict(vm, scope) {
        Some(dict) => dict,
        None => return Vec::new(),
    };
    let types = &vm.ctx.types;
    dict.into_iter()
        .filter_map(|(key, value)| {
            let name = key.try_to_value::<String>(vm).ok()?;
            let is_simple = is_simple_type(vm, &value);
            let is_string = value
                .is_instance(types.str_type.as_ref(), vm)
                .unwrap_or(false);
            let string_value = if is_string {
                value.try_to_value::<String>(vm).ok()
            } else {
                None
            };
            let repr = value
                .repr(vm)
                .map(|py_str| py_str.as_str().to_owned())
                .unwrap_or_else(|_| format!("<{}>", value.class().name()));
            Some(LocalValue {
                name,
                repr,
                is_simple,
                string_value,
            })
        })
        .collect()
}

fn collect_locals_map(vm: &vm::VirtualMachine, scope: &Scope) -> Vec<(String, String)> {
    let dict = match get_locals_dict(vm, scope) {
        Some(dict) => dict,
        None => return Vec::new(),
    };
    dict.into_iter()
        .filter_map(|(key, value)| {
            let name = key.try_to_value::<String>(vm).ok()?;
            let repr = value
                .repr(vm)
                .map(|py_str| py_str.as_str().to_owned())
                .unwrap_or_else(|_| format!("<{}>", value.class().name()));
            Some((name, repr))
        })
        .collect()
}

fn is_simple_type(vm: &vm::VirtualMachine, value: &vm::PyObjectRef) -> bool {
    let types = &vm.ctx.types;
    let candidates = [
        types.str_type.as_ref(),
        types.int_type.as_ref(),
        types.float_type.as_ref(),
        types.bool_type.as_ref(),
        types.list_type.as_ref(),
        types.dict_type.as_ref(),
        types.tuple_type.as_ref(),
    ];
    candidates
        .iter()
        .any(|ty| value.is_instance(ty, vm).unwrap_or(false))
}

fn parse_llm_prompt(prompt: &str) -> Vec<Message> {
    match serde_json::from_str::<serde_json::Value>(prompt) {
        Ok(value) => messages_from_json(value).unwrap_or_else(|| vec![Message::user(prompt)]),
        Err(_) => vec![Message::user(prompt)],
    }
}

fn messages_from_json(value: serde_json::Value) -> Option<Vec<Message>> {
    match value {
        serde_json::Value::Array(items) => {
            let mut messages = Vec::new();
            for item in items {
                if let serde_json::Value::String(text) = item {
                    messages.push(Message::user(text));
                    continue;
                }
                if let serde_json::Value::Object(map) = item
                    && let Some(message) = message_from_map(&map)
                {
                    messages.push(message);
                    continue;
                }
                return None;
            }
            Some(messages)
        }
        serde_json::Value::Object(map) => {
            if let Some(messages) = map.get("messages") {
                return messages_from_json(messages.clone());
            }
            message_from_map(&map).map(|msg| vec![msg])
        }
        serde_json::Value::String(text) => Some(vec![Message::user(text)]),
        _ => None,
    }
}

fn message_from_map(map: &serde_json::Map<String, serde_json::Value>) -> Option<Message> {
    let content_value = map.get("content")?;
    let content = match content_value {
        serde_json::Value::String(text) => text.to_owned(),
        other => other.to_string(),
    };
    let role = map
        .get("role")
        .and_then(|value| value.as_str())
        .unwrap_or("user")
        .to_owned();
    Some(Message { role, content })
}
