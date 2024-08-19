// SPDX-FileCopyrightText: 2024 Greenbone AG
//
// SPDX-License-Identifier: GPL-2.0-or-later WITH x11vnc-openssl-exception

use models::Scan;
use nasl_syntax::{Loader, NaslValue};
use storage::types::Primitive;
use storage::{ContextKey, Retriever, Storage};

use crate::scanner::ScannerStack;
use crate::scheduling::{ConcurrentVT, ConcurrentVTResult};

use super::error::{ExecuteError, ScriptResult, ScriptResultKind};
use super::scanner_stack::Schedule;

/// TODO: doc
pub struct ScanRunner<'a, T, S: ScannerStack> {
    schedule: T,
    scan: &'a models::Scan,

    /// Default Retriever
    storage: &'a S::Storage,
    /// Default Loader
    loader: &'a S::Loader,
    executor: &'a S::Executor,
    /// Is used to remember which host we currently are executing. The host name will get through
    /// the stored scan reference.
    current_host: usize,
    /// The first value is the stage and the second the vt idx and is used in combincation with
    /// current_host
    ///
    /// This is necessary after the first host. Internally we use schedule and iterate over it,
    /// when there is no error then we store it within concurrent vts. After the first host is done
    /// we cached all schedule results and switch to the next host. To not have to reschedule we
    /// keep track of the position
    current_host_concurrent_vt_idx: (usize, usize),
    /// We cache the results of the scheduler
    concurrent_vts: Vec<ConcurrentVT>,
}

impl<'a, Sched, Stack: ScannerStack> ScanRunner<'a, Sched, Stack>
where
    Sched: Schedule + 'a,
{
    /// TODO doc
    pub fn new(
        storage: &'a Stack::Storage,
        loader: &'a Stack::Loader,
        executor: &'a Stack::Executor,
        schedule: Sched,
        scan: &'a Scan,
    ) -> Self {
        Self {
            schedule,
            scan,
            storage,
            loader,
            executor,
            concurrent_vts: vec![],
            current_host: 0,
            current_host_concurrent_vt_idx: (0, 0),
        }
    }

    fn parameter(
        &mut self,
        parameter: &models::Parameter,
        _register: &mut crate::Register,
    ) -> Result<(), ExecuteError> {
        // TODO: implement
        Err(ExecuteError::Parameter(parameter.clone()))
    }

    fn check_key<A, B, C>(
        &self,
        key: &storage::ContextKey,
        kb_key: &str,
        result_none: A,
        result_some: B,
        result_err: C,
    ) -> Result<(), ScriptResultKind>
    where
        A: Fn() -> Option<ScriptResultKind>,
        B: Fn(Primitive) -> Option<ScriptResultKind>,
        C: Fn(storage::StorageError) -> Option<ScriptResultKind>,
    {
        let _span = tracing::error_span!("kb_item", %key, kb_key).entered();
        let result = match self
            .storage
            .retrieve(key, storage::Retrieve::KB(kb_key.to_string()))
        {
            Ok(mut x) => {
                let x = x.next();
                if let Some(x) = x {
                    match x {
                        storage::Field::KB(kb) => {
                            tracing::trace!(value=?kb.value, "found");
                            result_some(kb.value)
                        }
                        x => {
                            tracing::trace!(field=?x, "found but it is not a KB item");
                            result_none()
                        }
                    }
                } else {
                    tracing::trace!("not found");
                    result_none()
                }
            }
            Err(e) => {
                tracing::warn!(error=%e, "storage error");
                result_err(e)
            }
        };
        match result {
            None => Ok(()),
            Some(x) => Err(x),
        }
    }

    fn check_keys(&self, vt: &storage::item::Nvt) -> Result<(), ScriptResultKind> {
        let key = self.generate_key();
        let check_required_key = |k: &str| {
            self.check_key(
                &key,
                k,
                || Some(ScriptResultKind::MissingRequiredKey(k.into())),
                |_| None,
                |_| Some(ScriptResultKind::MissingRequiredKey(k.into())),
            )
        };
        for k in &vt.required_keys {
            check_required_key(k)?
        }

        let check_mandatory_key = |k: &str| {
            self.check_key(
                &key,
                k,
                || Some(ScriptResultKind::MissingMandatoryKey(k.into())),
                |_| None,
                |_| Some(ScriptResultKind::MissingMandatoryKey(k.into())),
            )
        };
        for k in &vt.mandatory_keys {
            check_mandatory_key(k)?
        }

        let check_exclude_key = |k: &str| {
            self.check_key(
                &key,
                k,
                || None,
                |_| Some(ScriptResultKind::ContainsExcludedKey(k.into())),
                |_| None,
            )
        };
        for k in &vt.excluded_keys {
            check_exclude_key(k)?
        }

        use models::Protocol;
        let check_port = |pt: Protocol, port: &str| {
            let kbk = generate_port_kb_key(pt, port);
            self.check_key(
                &key,
                &kbk,
                || Some(ScriptResultKind::MissingPort(pt, port.to_string())),
                |v| {
                    if v.into() {
                        None
                    } else {
                        Some(ScriptResultKind::MissingPort(pt, port.to_string()))
                    }
                },
                |_| Some(ScriptResultKind::MissingPort(pt, port.to_string())),
            )
        };
        for k in &vt.required_ports {
            check_port(Protocol::TCP, k)?
        }
        for k in &vt.required_udp_ports {
            check_port(Protocol::UDP, k)?
        }

        Ok(())
    }

    // TODO: probably better to enhance ContextKey::Scan to contain target and scan_id?
    fn generate_key(&self) -> ContextKey {
        let target = &self.scan.target.hosts[self.current_host].to_string();
        let scan_id = &self.scan.scan_id;
        ContextKey::Scan(scan_id.clone(), Some(target.clone()))
    }

    fn execute(
        &mut self,
        stage: crate::scheduling::Stage,
        vt: storage::item::Nvt,
        param: Option<Vec<models::Parameter>>,
    ) -> Result<ScriptResult, ExecuteError> {
        let code = self.loader.load(&vt.filename)?;
        let target = self.scan.target.hosts[self.current_host].to_string();
        let mut register = crate::Register::default();
        if let Some(params) = param {
            for p in params.iter() {
                self.parameter(p, &mut register)?;
            }
        }

        let _span = tracing::span!(
            tracing::Level::WARN,
            "executing",
            filename = &vt.filename,
            oid = &vt.oid,
            %stage,
            target,
        )
        .entered();

        // currently scans are limited to the target as well as the id.
        tracing::debug!("running");
        let kind = {
            match self.check_keys(&vt) {
                Err(e) => e,
                Ok(()) => {
                    let context = crate::Context::new(
                        self.generate_key(),
                        target.clone(),
                        self.storage.as_dispatcher(),
                        self.storage.as_retriever(),
                        self.loader,
                        self.executor,
                    );
                    let mut interpret = crate::CodeInterpreter::new(&code, register, &context);

                    interpret
                        .find_map(|r| match r {
                            Ok(NaslValue::Exit(x)) => Some(ScriptResultKind::ReturnCode(x)),
                            Err(e) => Some(ScriptResultKind::Error(e.clone())),
                            Ok(x) => {
                                tracing::trace!(statement_result=?x);
                                None
                            }
                        })
                        .unwrap_or_else(|| ScriptResultKind::ReturnCode(0))
                }
            }
        };
        tracing::debug!(result=?kind, "finished");
        Ok(ScriptResult {
            oid: vt.oid,
            filename: vt.filename,
            stage,
            kind,
            target,
        })
    }

    /// Checks if current current_host_concurrent_vt_idx as well as current_host are valid and may
    /// adapt them. Returns None when there are no hosts left.
    fn sanitize_indeces(&mut self) -> Option<Result<(usize, (usize, usize)), ExecuteError>> {
        let (mut si, mut vi) = self.current_host_concurrent_vt_idx;
        let mut hi = self.current_host;
        if self.current_host == 0 {
            // we cache all staging steps so that we can iterator through all vts per hosts.
            // this is easier to handle for the callter as they can
            match self.schedule.next() {
                Some(next) => {
                    match next {
                        Ok(next) => {
                            self.concurrent_vts.push(next);
                        }
                        Err(e) => {
                            // Note: if the caller ignores the error and continues then the
                            // VT will be skipped and may result in unpredictable behaviour
                            // in the following runs. An alternative approach would be to
                            // go to the next host. That way the run would stop at the
                            // fauly scheduling for each run instead of trying to continue.
                            return Some(Err(e.into()));
                        }
                    }
                }
                None => {
                    // finished first run
                }
            }
        }
        let new_host = si >= self.concurrent_vts.len()
            || (vi >= self.concurrent_vts[si].1.len() && si + 1 >= self.concurrent_vts.len());
        if new_host {
            if let Err(e) = self.storage.scan_finished(&self.generate_key()) {
                return Some(Err(e.into()));
            }
            si = 0;
            vi = 0;
            hi += 1;
        } else if vi >= self.concurrent_vts[si].1.len() {
            // new_stage
            si += 1;
            vi = 0;
        }

        if hi < self.scan.target.hosts.len() {
            self.current_host = hi;
            self.current_host_concurrent_vt_idx = (si, vi);
            Some(Ok((hi, (si, vi))))
        } else {
            None
        }
    }
}

impl<'a, T, S: ScannerStack> Iterator for ScanRunner<'a, T, S>
where
    T: Iterator<Item = ConcurrentVTResult> + 'a,
{
    type Item = Result<ScriptResult, ExecuteError>;

    fn next(&mut self) -> Option<Self::Item> {
        let (_, (si, vi)) = match self.sanitize_indeces()? {
            Ok(x) => x,
            Err(e) => {
                self.current_host_concurrent_vt_idx = (
                    self.current_host_concurrent_vt_idx.0,
                    self.current_host_concurrent_vt_idx.1 + 1,
                );
                return Some(Err(e));
            }
        };

        let (stage, vts) = &self.concurrent_vts[si];
        let (vt, param) = &vts[vi];

        self.current_host_concurrent_vt_idx = (si, vi + 1);

        Some(self.execute(stage.clone(), vt.clone(), param.clone()))
    }
}

pub(crate) fn generate_port_kb_key(protocol: models::Protocol, port: &str) -> String {
    format!("Ports/{protocol}/{port}")
}

#[cfg(test)]
pub(super) mod tests {
    use nasl_builtin_utils::NaslFunctionRegister;
    use storage::item::Nvt;
    use storage::Dispatcher;
    use storage::Retriever;

    use crate::nasl_std_functions;
    use crate::scanner::error::ExecuteError;
    use crate::scanner::error::ScriptResult;
    use crate::scanner::scan_runner::ScanRunner;
    use crate::scheduling::ExecutionPlaner;
    use crate::scheduling::WaveExecutionPlan;

    pub fn only_success() -> [(String, Nvt); 3] {
        [
            GenerateScript::with_dependencies("0", &[]).generate(),
            GenerateScript::with_dependencies("1", &["0.nasl"]).generate(),
            GenerateScript::with_dependencies("2", &["1.nasl"]).generate(),
        ]
    }

    fn loader(s: &str) -> String {
        let only_success = only_success();
        let stou = |s: &str| s.split('.').next().unwrap().parse::<usize>().unwrap();
        only_success[stou(s)].0.clone()
    }

    pub fn setup(
        scripts: &[(String, storage::item::Nvt)],
    ) -> (
        (
            storage::DefaultDispatcher,
            fn(&str) -> String,
            NaslFunctionRegister,
        ),
        models::Scan,
    ) {
        use storage::Dispatcher;
        let storage = storage::DefaultDispatcher::new();
        scripts.iter().map(|(_, v)| v).for_each(|n| {
            storage
                .dispatch(
                    &storage::ContextKey::FileName(n.filename.clone()),
                    storage::Field::NVT(storage::item::NVTField::Nvt(n.clone())),
                )
                .expect("sending")
        });
        let scan = models::Scan {
            scan_id: "sid".to_string(),
            target: models::Target {
                hosts: vec!["test.host".to_string()],
                ..Default::default()
            },
            scan_preferences: vec![],
            vts: scripts
                .iter()
                .map(|(_, v)| models::VT {
                    oid: v.oid.clone(),
                    parameters: vec![],
                })
                .collect(),
        };
        let executor = nasl_std_functions();
        ((storage, loader, executor), scan)
    }

    pub fn setup_success() -> (
        (
            storage::DefaultDispatcher,
            fn(&str) -> String,
            NaslFunctionRegister,
        ),
        models::Scan,
    ) {
        setup(&only_success())
    }

    #[derive(Debug, Default)]
    pub struct GenerateScript {
        pub id: String,
        pub rc: usize,
        pub dependencies: Vec<String>,
        pub required_keys: Vec<String>,
        pub mandatory_keys: Vec<String>,
        pub required_tcp_ports: Vec<String>,
        pub required_udp_ports: Vec<String>,
        pub exclude: Vec<String>,
    }

    impl GenerateScript {
        pub fn with_dependencies(id: &str, dependencies: &[&str]) -> GenerateScript {
            let dependencies = dependencies.iter().map(|x| x.to_string()).collect();

            GenerateScript {
                id: id.to_string(),
                dependencies,
                ..Default::default()
            }
        }

        pub fn with_required_keys(id: &str, required_keys: &[&str]) -> GenerateScript {
            let required_keys = required_keys.iter().map(|x| x.to_string()).collect();
            GenerateScript {
                id: id.to_string(),
                required_keys,
                ..Default::default()
            }
        }

        pub fn with_mandatory_keys(id: &str, mandatory_keys: &[&str]) -> GenerateScript {
            let mandatory_keys = mandatory_keys.iter().map(|x| x.to_string()).collect();
            GenerateScript {
                id: id.to_string(),
                mandatory_keys,
                ..Default::default()
            }
        }

        pub fn with_excluded_keys(id: &str, exclude_keys: &[&str]) -> GenerateScript {
            let exclude = exclude_keys.iter().map(|x| x.to_string()).collect();
            GenerateScript {
                id: id.to_string(),
                exclude,
                ..Default::default()
            }
        }

        pub fn with_required_ports(id: &str, ports: &[(models::Protocol, &str)]) -> GenerateScript {
            let required_tcp_ports = ports
                .iter()
                .filter(|(p, _)| matches!(p, models::Protocol::TCP))
                .map(|(_, p)| p.to_string())
                .collect();
            let required_udp_ports = ports
                .iter()
                .filter(|(p, _)| matches!(p, models::Protocol::UDP))
                .map(|(_, p)| p.to_string())
                .collect();

            GenerateScript {
                id: id.to_string(),
                required_tcp_ports,
                required_udp_ports,

                ..Default::default()
            }
        }

        pub fn generate(&self) -> (String, storage::item::Nvt) {
            let keys = |x: &[String]| -> String {
                x.iter().fold(String::default(), |acc, e| {
                    let acc = if acc.is_empty() {
                        acc
                    } else {
                        format!("{acc}, ")
                    };
                    format!("\"{acc}{e}\"")
                })
            };
            let printable = |name, x| -> String {
                let dependencies = keys(x);
                if dependencies.is_empty() {
                    String::default()
                } else {
                    format!("{name}({dependencies});")
                }
            };
            let mandatory = printable("script_mandatory_keys", &self.mandatory_keys);
            let required = printable("script_require_keys", &self.required_keys);
            let dependencies = printable("script_dependencies", &self.dependencies);
            let exclude = printable("script_exclude_keys", &self.exclude);
            let require_ports = printable("script_require_ports", &self.required_tcp_ports);
            let require_udp_ports = printable("script_require_udp_ports", &self.required_udp_ports);

            let rc = self.rc;
            let id = &self.id;

            let code = format!(
                r#"
if (description)
{{
  script_oid("{id}");
  script_category(ACT_GATHER_INFO);
  {dependencies}
  {mandatory}
  {required}
  {exclude}
  {require_ports}
  {require_udp_ports}
  exit(0);
}}
log_message(data: "Ja, junge dat is Kaffee, echt jetzt, und Kaffee ist nun mal lecker.");
exit({rc});
"#
            );
            let filename = format!("{id}.nasl");
            let nvt = parse_meta_data(&filename, &code).expect("expected metadata");
            (code, nvt)
        }
    }

    fn parse_meta_data(id: &str, code: &str) -> Option<storage::item::Nvt> {
        let initial = vec![
            ("description".to_owned(), true.into()),
            ("OPENVAS_VERSION".to_owned(), "testus".into()),
        ];
        let storage = storage::DefaultDispatcher::new();

        let register = nasl_builtin_utils::Register::root_initial(&initial);
        let target = String::default();
        let functions = nasl_std_functions();
        let loader = |_: &str| code.to_string();
        let key = storage::ContextKey::FileName(id.to_string());

        let context =
            nasl_builtin_utils::Context::new(key, target, &storage, &storage, &loader, &functions);
        let interpreter = crate::CodeInterpreter::new(code, register, &context);
        for stmt in interpreter.iter_blocking() {
            if let nasl_syntax::NaslValue::Exit(_) = stmt.expect("stmt success") {
                storage.on_exit(context.key()).expect("result");
                let result = storage
                    .retrieve(
                        &storage::ContextKey::FileName(id.to_string()),
                        storage::Retrieve::NVT(None),
                    )
                    .expect("nvt for id")
                    .next();
                if let Some(storage::Field::NVT(storage::item::NVTField::Nvt(nvt))) = result {
                    return Some(nvt);
                } else {
                    return None;
                }
            }
        }
        None
    }

    fn prepare_vt_storage(scripts: &[(String, storage::item::Nvt)]) -> storage::DefaultDispatcher {
        let dispatcher = storage::DefaultDispatcher::new();
        scripts.iter().map(|(_, v)| v).for_each(|n| {
            dispatcher
                .dispatch(
                    &storage::ContextKey::FileName(n.filename.clone()),
                    storage::Field::NVT(storage::item::NVTField::Nvt(n.clone())),
                )
                .expect("sending")
        });
        dispatcher
    }

    fn run(
        scripts: Vec<(String, storage::item::Nvt)>,
        storage: storage::DefaultDispatcher,
    ) -> Result<Vec<Result<ScriptResult, ExecuteError>>, ExecuteError> {
        let stou = |s: &str| s.split('.').next().unwrap().parse::<usize>().unwrap();
        let loader_scripts = scripts.clone();
        let loader = move |s: &str| loader_scripts[stou(s)].0.clone();
        let scan = models::Scan {
            scan_id: "sid".to_string(),
            target: models::Target {
                hosts: vec!["test.host".to_string()],
                ..Default::default()
            },
            scan_preferences: vec![],
            vts: scripts
                .iter()
                .map(|(_, v)| models::VT {
                    oid: v.oid.clone(),
                    parameters: vec![],
                })
                .collect(),
        };

        let executor = nasl_std_functions();

        let schedule = storage.execution_plan::<WaveExecutionPlan>(&scan)?;
        let interpreter: ScanRunner<_, (_, _, _)> =
            ScanRunner::new(&storage, &loader, &executor, schedule, &scan);
        let results = interpreter.collect::<Vec<_>>();
        Ok(results)
    }

    #[test]
    #[tracing_test::traced_test]
    fn required_ports() {
        let vts = [
            GenerateScript::with_required_ports(
                "0",
                &[
                    (models::Protocol::UDP, "2000"),
                    (models::Protocol::TCP, "20"),
                ],
            )
            .generate(),
            GenerateScript::with_required_ports(
                "1",
                &[
                    (models::Protocol::UDP, "2000"),
                    (models::Protocol::TCP, "2"),
                ],
            )
            .generate(),
            GenerateScript::with_required_ports(
                "2",
                &[
                    (models::Protocol::UDP, "200"),
                    (models::Protocol::TCP, "20"),
                ],
            )
            .generate(),
            GenerateScript::with_required_ports(
                "3",
                &[
                    (models::Protocol::UDP, "2000"),
                    (models::Protocol::TCP, "22"),
                ],
            )
            .generate(),
            GenerateScript::with_required_ports(
                "4",
                &[
                    (models::Protocol::UDP, "2002"),
                    (models::Protocol::TCP, "20"),
                ],
            )
            .generate(),
        ];
        let dispatcher = prepare_vt_storage(&vts);
        [
            (models::Protocol::TCP, "20", 1),   // TCP 20 is considered enabled
            (models::Protocol::TCP, "22", 0),   // TCP 22 is considered disabled
            (models::Protocol::UDP, "2000", 1), // UDP 2000 is considered enabled
            (models::Protocol::UDP, "2002", 0), // UDP 2002 is considered disabled
        ]
        .into_iter()
        .for_each(|(p, port, enabled)| {
            dispatcher
                .dispatch(
                    &storage::ContextKey::Scan("sid".into(), Some("test.host".into())),
                    storage::Field::KB((&super::generate_port_kb_key(p, port), enabled).into()),
                )
                .expect("store kb");
        });
        let result = run(vts.to_vec(), dispatcher).expect("success run");
        let success = result
            .clone()
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_succeeded())
            .collect::<Vec<_>>();
        let failure = result
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_failed())
            .filter(|x| x.has_not_run())
            .collect::<Vec<_>>();
        assert_eq!(success.len(), 1);
        assert_eq!(failure.len(), 4);
    }

    #[test]
    #[tracing_test::traced_test]
    fn exclude_keys() {
        let only_success = [
            GenerateScript::with_excluded_keys("0", &["key/not"]).generate(),
            GenerateScript::with_excluded_keys("1", &["key/not"]).generate(),
            GenerateScript::with_excluded_keys("2", &["key/exists"]).generate(),
        ];
        let dispatcher = prepare_vt_storage(&only_success);
        dispatcher
            .dispatch(
                &storage::ContextKey::Scan("sid".into(), Some("test.host".into())),
                storage::Field::KB(("key/exists", 1).into()),
            )
            .expect("store kb");
        let result = run(only_success.to_vec(), dispatcher).expect("success run");
        let success = result
            .clone()
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_succeeded())
            .collect::<Vec<_>>();
        let failure = result
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_failed())
            .filter(|x| x.has_not_run())
            .collect::<Vec<_>>();
        assert_eq!(success.len(), 2);
        assert_eq!(failure.len(), 1);
    }

    #[test]
    #[tracing_test::traced_test]
    fn required_keys() {
        let only_success = [
            GenerateScript::with_required_keys("0", &["key/not"]).generate(),
            GenerateScript::with_required_keys("1", &["key/exists"]).generate(),
        ];
        let dispatcher = prepare_vt_storage(&only_success);
        dispatcher
            .dispatch(
                &storage::ContextKey::Scan("sid".into(), Some("test.host".into())),
                storage::Field::KB(("key/exists", 1).into()),
            )
            .expect("store kb");
        let result = run(only_success.to_vec(), dispatcher).expect("success run");
        let success = result
            .clone()
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_succeeded())
            .collect::<Vec<_>>();
        let failure = result
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_failed())
            .filter(|x| x.has_not_run())
            .collect::<Vec<_>>();
        assert_eq!(success.len(), 1);
        assert_eq!(failure.len(), 1);
    }

    #[test]
    #[tracing_test::traced_test]
    fn mandatory_keys() {
        let only_success = [
            GenerateScript::with_mandatory_keys("0", &["key/not"]).generate(),
            GenerateScript::with_mandatory_keys("1", &["key/exists"]).generate(),
        ];
        let dispatcher = prepare_vt_storage(&only_success);
        dispatcher
            .dispatch(
                &storage::ContextKey::Scan("sid".into(), Some("test.host".to_string())),
                storage::Field::KB(("key/exists", 1).into()),
            )
            .expect("store kb");
        let result = run(only_success.to_vec(), dispatcher).expect("success run");
        let success = result
            .clone()
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_succeeded())
            .collect::<Vec<_>>();
        let failure = result
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.has_failed())
            .filter(|x| x.has_not_run())
            .collect::<Vec<_>>();
        assert_eq!(success.len(), 1);
        assert_eq!(failure.len(), 1);
    }
}
