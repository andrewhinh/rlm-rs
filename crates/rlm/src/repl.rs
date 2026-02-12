use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use async_trait::async_trait;
use rustpython_pylib;
use rustpython_stdlib;
use rustpython_vm as vm;
use rustpython_vm::builtins::{PyBaseException, PyDictRef};
use rustpython_vm::scope::Scope;
use rustpython_vm::{Interpreter, InterpreterBuilder};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use crate::llm::{LlmClient, Message};
use crate::utils::{ContextData, ContextInput, context_from_value};

#[async_trait]
pub trait RecursiveRunner: Send + Sync {
    async fn completion(&self, query: String, context: ContextInput) -> anyhow::Result<String>;
}

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

#[derive(Debug, Deserialize)]
struct RlmQueryPayload {
    query: Option<String>,
    context: Option<Value>,
}

const EXECUTION_TIMEOUT_SECS: f64 = 10.0;
const MAX_SUBCALL_TOTAL_TOKENS_APPROX: usize = 120_000;
const MAX_SUBCALL_MESSAGE_TOKENS_APPROX: usize = 105_000;
const MAX_SUBCALL_TOTAL_CHARS: usize = 480_000;
const MAX_SUBCALL_MESSAGE_CHARS: usize = 420_000;

enum ReplCommand {
    Init {
        context: ContextData,
        setup_code: Option<String>,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    Execute {
        code: String,
        response: oneshot::Sender<anyhow::Result<ReplResult>>,
    },
    GetVariable {
        name: String,
        response: oneshot::Sender<anyhow::Result<Option<String>>>,
    },
    Reset {
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    Shutdown {
        response: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub struct ReplHandle {
    sender: mpsc::UnboundedSender<ReplCommand>,
}

struct ReplCore {
    llm_client: Arc<dyn LlmClient>,
    runtime_handle: Handle,
    recursive_runner: Option<Arc<dyn RecursiveRunner>>,
    recursion_depth: usize,
    repl_env: Option<ReplEnv>,
}

pub struct ReplEnv {
    interpreter: Interpreter,
    scope: Scope,
    temp_dir: TempDir,
    llm_client: Arc<dyn LlmClient>,
    runtime_handle: Handle,
    recursive_runner: Option<Arc<dyn RecursiveRunner>>,
    recursion_depth: usize,
    execution_lock: Mutex<()>,
}

impl ReplEnv {
    pub fn new(
        context: ContextData,
        llm_client: Arc<dyn LlmClient>,
        recursive_runner: Option<Arc<dyn RecursiveRunner>>,
        recursion_depth: usize,
        setup_code: Option<&str>,
        runtime_handle: Handle,
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
            runtime_handle,
            recursive_runner,
            recursion_depth,
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
        let runtime_handle = self.runtime_handle.clone();
        let recursive_runner = self.recursive_runner.clone();
        let recursion_depth = self.recursion_depth;
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
            let llm_runtime_handle = runtime_handle.clone();
            let llm_fn = vm.new_function(
                "__rlm_llm_query",
                move |prompt: String| -> vm::PyResult<String> {
                    let messages = parse_llm_prompt(&prompt);
                    if let Err(err) = validate_subcall_messages(&messages) {
                        return Ok(format!("Error making LLM query: {err}"));
                    }
                    let llm_client = llm_client.clone();
                    let runtime_handle = llm_runtime_handle.clone();
                    let response = runtime_handle.block_on(async move {
                        llm_client
                            .completion(&messages, None)
                            .await
                            .unwrap_or_else(|err| format!("Error making LLM query: {err}"))
                    });
                    Ok(response)
                },
            );
            scope
                .globals
                .set_item("__rlm_llm_query", llm_fn.into(), vm)?;
            let recursive_runner_many = recursive_runner.clone();
            let rlm_runtime_handle = runtime_handle.clone();
            let rlm_fn = vm.new_function(
                "__rlm_rlm_query",
                move |payload_json: String| -> vm::PyResult<String> {
                    if recursion_depth == 0 || recursive_runner_many.is_none() {
                        return Ok(
                            "Error: rlm_query disabled at depth 0; increase depth to enable."
                                .to_owned(),
                        );
                    }
                    let payloads: Vec<RlmQueryPayload> = match serde_json::from_str(&payload_json)
                    {
                        Ok(payloads) => payloads,
                        Err(err) => {
                            return Ok(format!("Error parsing rlm_query payloads: {err}"));
                        }
                    };
                    if payloads.is_empty() {
                        return Ok("[]".to_owned());
                    }
                    let runner = recursive_runner_many
                        .clone()
                        .expect("recursive runner");
                    let runtime_handle = rlm_runtime_handle.clone();
                    let outputs = runtime_handle.block_on(async move {
                        let mut outputs = Vec::with_capacity(payloads.len());
                        for payload in payloads {
                            let query = payload
                                .query
                                .unwrap_or_else(|| crate::prompts::DEFAULT_QUERY.to_owned());
                            let context = context_from_value(payload.context);
                            let result = runner.completion(query, context).await;
                            match result {
                                Ok(result) => outputs.push(result),
                                Err(err) => outputs.push(format!("Error running rlm_query: {err}")),
                            }
                        }
                        outputs
                    });
                    Ok(serde_json::to_string(&outputs).unwrap_or_else(|_| "[]".to_owned()))
                },
            );
            scope
                .globals
                .set_item("__rlm_rlm_query", rlm_fn.into(), vm)?;
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
                    r#"__rlm_json = __rlm_get_builtin('__import__')('json')
__rlm_sys = __rlm_get_builtin('__import__')('sys')

def llm_query(prompts):
    if isinstance(prompts, list):
        payload = __rlm_json.dumps(prompts, default=str)
    else:
        payload = __rlm_json.dumps([prompts], default=str)
    __rlm_gettrace = getattr(__rlm_sys, 'gettrace', None)
    __rlm_settrace = getattr(__rlm_sys, 'settrace', None)
    prev_trace = None
    if __rlm_settrace is not None:
        prev_trace = __rlm_gettrace() if __rlm_gettrace is not None else None
        __rlm_settrace(None)
    try:
        return __rlm_llm_query(payload)
    finally:
        if __rlm_settrace is not None:
            __rlm_settrace(prev_trace)
"#,
                ),
                (
                    "rlm_query",
                    r#"def rlm_query(query, context=None):
    if isinstance(query, list) and context is None:
        items = query
        unwrap_single = False
    else:
        items = [query]
        unwrap_single = True
    __rlm_json = __rlm_get_builtin('__import__')('json')
    __rlm_globals = __rlm_globals_builtin()
    payload_items = []
    for item in items:
        if isinstance(item, dict):
            q = item.get("query")
            ctx = item.get("context")
        elif isinstance(item, (list, tuple)) and len(item) == 2:
            q, ctx = item
        else:
            q = item
            ctx = context
        if ctx is None:
            ctx = context
        if ctx is None:
            ctx = __rlm_globals.get("context")
        payload_items.append({"query": str(q), "context": ctx})
    payload = __rlm_json.dumps(payload_items, default=str)
    response = __rlm_rlm_query(payload)
    try:
        parsed = __rlm_json.loads(response)
    except Exception:
        return response
    if unwrap_single and isinstance(parsed, list) and len(parsed) == 1:
        return parsed[0]
    return parsed
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

impl ReplCore {
    fn new(
        llm_client: Arc<dyn LlmClient>,
        runtime_handle: Handle,
        recursive_runner: Option<Arc<dyn RecursiveRunner>>,
        recursion_depth: usize,
    ) -> Self {
        Self {
            llm_client,
            runtime_handle,
            recursive_runner,
            recursion_depth,
            repl_env: None,
        }
    }

    fn init(&mut self, context: ContextData, setup_code: Option<String>) -> anyhow::Result<()> {
        let env = ReplEnv::new(
            context,
            self.llm_client.clone(),
            self.recursive_runner.clone(),
            self.recursion_depth,
            setup_code.as_deref(),
            self.runtime_handle.clone(),
        )?;
        self.repl_env = Some(env);
        Ok(())
    }

    fn execute(&mut self, code: String) -> anyhow::Result<ReplResult> {
        let repl_env = self
            .repl_env
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;
        repl_env.execute(&code)
    }

    fn get_variable(&self, name: String) -> anyhow::Result<Option<String>> {
        let repl_env = self
            .repl_env
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("repl env not initialized"))?;
        repl_env.get_variable(&name)
    }

    fn reset(&mut self) {
        self.repl_env = None;
    }
}

impl ReplHandle {
    pub fn new(
        llm_client: Arc<dyn LlmClient>,
        recursive_runner: Option<Arc<dyn RecursiveRunner>>,
        recursion_depth: usize,
    ) -> anyhow::Result<Self> {
        let runtime_handle = Handle::try_current()
            .map_err(|err| anyhow::anyhow!("tokio runtime handle unavailable: {err}"))?;
        let (sender, mut receiver) = mpsc::unbounded_channel();

        thread::Builder::new()
            .name("rlm-repl-worker".to_owned())
            .spawn(move || {
                let mut core = ReplCore::new(
                    llm_client,
                    runtime_handle,
                    recursive_runner,
                    recursion_depth,
                );
                while let Some(command) = receiver.blocking_recv() {
                    match command {
                        ReplCommand::Init {
                            context,
                            setup_code,
                            response,
                        } => {
                            let _ = response.send(core.init(context, setup_code));
                        }
                        ReplCommand::Execute { code, response } => {
                            let _ = response.send(core.execute(code));
                        }
                        ReplCommand::GetVariable { name, response } => {
                            let _ = response.send(core.get_variable(name));
                        }
                        ReplCommand::Reset { response } => {
                            core.reset();
                            let _ = response.send(Ok(()));
                        }
                        ReplCommand::Shutdown { response } => {
                            let _ = response.send(());
                            break;
                        }
                    }
                }
            })?;

        Ok(Self { sender })
    }

    pub async fn init(
        &self,
        context: ContextData,
        setup_code: Option<String>,
    ) -> anyhow::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(ReplCommand::Init {
                context,
                setup_code,
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("failed to send init command to repl worker"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("repl worker dropped init response"))?
    }

    pub async fn execute(&self, code: String) -> anyhow::Result<ReplResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(ReplCommand::Execute {
                code,
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("failed to send execute command to repl worker"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("repl worker dropped execute response"))?
    }

    pub async fn get_variable(&self, name: String) -> anyhow::Result<Option<String>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(ReplCommand::GetVariable {
                name,
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("failed to send get_variable command to repl worker"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("repl worker dropped get_variable response"))?
    }

    pub async fn reset(&self) -> anyhow::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(ReplCommand::Reset {
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("failed to send reset command to repl worker"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("repl worker dropped reset response"))?
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(ReplCommand::Shutdown {
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("failed to send shutdown command to repl worker"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("repl worker dropped shutdown response"))?;
        Ok(())
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

fn validate_subcall_messages(messages: &[Message]) -> Result<(), String> {
    let total_chars: usize = messages.iter().map(|msg| msg.content.len()).sum();
    let total_tokens_approx = estimate_tokens(total_chars);
    if total_chars > MAX_SUBCALL_TOTAL_CHARS {
        return Err(format!(
            "sub-query too large ({total_chars} chars > {MAX_SUBCALL_TOTAL_CHARS}). Chunk the context before calling llm_query."
        ));
    }
    if total_tokens_approx > MAX_SUBCALL_TOTAL_TOKENS_APPROX {
        return Err(format!(
            "sub-query too large (~{total_tokens_approx} tokens > {MAX_SUBCALL_TOTAL_TOKENS_APPROX}). Chunk the context before calling llm_query."
        ));
    }
    if let Some(oversized) = messages
        .iter()
        .map(|msg| msg.content.len())
        .max()
        .filter(|len| *len > MAX_SUBCALL_MESSAGE_CHARS)
    {
        return Err(format!(
            "single sub-query message too large ({oversized} chars > {MAX_SUBCALL_MESSAGE_CHARS}). Chunk the context before calling llm_query."
        ));
    }
    if let Some(oversized_tokens) = messages
        .iter()
        .map(|msg| estimate_tokens(msg.content.len()))
        .max()
        .filter(|tokens| *tokens > MAX_SUBCALL_MESSAGE_TOKENS_APPROX)
    {
        return Err(format!(
            "single sub-query message too large (~{oversized_tokens} tokens > {MAX_SUBCALL_MESSAGE_TOKENS_APPROX}). Chunk the context before calling llm_query."
        ));
    }
    Ok(())
}

fn estimate_tokens(char_count: usize) -> usize {
    char_count.div_ceil(4)
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
