use crate::api::schema::{
    Method, Request, WorkflowCancelParams, WorkflowDeleteParams, WorkflowGetParams,
    WorkflowListParams, WorkflowNodePatch, WorkflowPauseParams, WorkflowResumeParams,
    WorkflowRunParams, WorkflowUpdateParams,
};

pub(super) fn run_workflow_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_workflow_help();
        return Ok(2);
    };

    match subcommand {
        "run" => workflow_run(&args[1..]),
        "list" => workflow_list(&args[1..]),
        "get" => workflow_get(&args[1..]),
        "status" => workflow_get(&args[1..]),
        "pause" => workflow_pause(&args[1..]),
        "resume" => workflow_resume(&args[1..]),
        "cancel" => workflow_cancel(&args[1..]),
        "delete" => workflow_delete(&args[1..]),
        "bind" => workflow_bind(&args[1..]),
        "help" | "--help" | "-h" => {
            print_workflow_help();
            Ok(0)
        }
        _ => {
            print_workflow_help();
            Ok(2)
        }
    }
}

fn single_arg(args: &[String], usage: &str) -> Option<String> {
    let Some(value) = args.first() else {
        eprintln!("usage: {usage}");
        return None;
    };
    if args.len() != 1 {
        eprintln!("usage: {usage}");
        return None;
    }
    Some(value.clone())
}

fn send(id: &'static str, method: Method) -> std::io::Result<i32> {
    super::print_response(&super::send_request(&Request {
        id: id.into(),
        method,
    })?)
}

fn workflow_run(args: &[String]) -> std::io::Result<i32> {
    let Some(path) = single_arg(args, "herdr workflow run <path.aflow.json>") else {
        return Ok(2);
    };
    send(
        "cli:workflow:run",
        Method::WorkflowRun(WorkflowRunParams { path }),
    )
}

fn workflow_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("unknown option: {}", args[0]);
        return Ok(2);
    }
    send(
        "cli:workflow:list",
        Method::WorkflowList(WorkflowListParams::default()),
    )
}

fn workflow_get(args: &[String]) -> std::io::Result<i32> {
    let Some(run_id) = single_arg(args, "herdr workflow get <run_id>") else {
        return Ok(2);
    };
    send(
        "cli:workflow:get",
        Method::WorkflowGet(WorkflowGetParams { run_id }),
    )
}

fn workflow_pause(args: &[String]) -> std::io::Result<i32> {
    let Some(run_id) = single_arg(args, "herdr workflow pause <run_id>") else {
        return Ok(2);
    };
    send(
        "cli:workflow:pause",
        Method::WorkflowPause(WorkflowPauseParams { run_id }),
    )
}

fn workflow_resume(args: &[String]) -> std::io::Result<i32> {
    let Some(run_id) = single_arg(args, "herdr workflow resume <run_id>") else {
        return Ok(2);
    };
    send(
        "cli:workflow:resume",
        Method::WorkflowResume(WorkflowResumeParams { run_id }),
    )
}

fn workflow_cancel(args: &[String]) -> std::io::Result<i32> {
    let Some(run_id) = single_arg(args, "herdr workflow cancel <run_id>") else {
        return Ok(2);
    };
    send(
        "cli:workflow:cancel",
        Method::WorkflowCancel(WorkflowCancelParams { run_id }),
    )
}

fn workflow_delete(args: &[String]) -> std::io::Result<i32> {
    let Some(run_id) = single_arg(args, "herdr workflow delete <run_id>") else {
        return Ok(2);
    };
    send(
        "cli:workflow:delete",
        Method::WorkflowDelete(WorkflowDeleteParams { run_id }),
    )
}

fn workflow_bind(args: &[String]) -> std::io::Result<i32> {
    let usage = "herdr workflow bind <path.aflow.json> --node <id> [--runtime <r>] \
                 [--profile <id>] [--pool <group>] [--model <m>] [--timeout-ms <n>] \
                 [--visible|--invisible] [--enable|--disable]";
    let mut path = None;
    let mut patch = WorkflowNodePatch {
        node_id: String::new(),
        runtime: None,
        provider_profile_id: None,
        provider_pool: None,
        model: None,
        timeout_ms: None,
        visible: None,
        enabled: None,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        index += 1;
        // Reads the value at the current position (the flag's argument).
        fn next_value(
            args: &[String],
            index: &mut usize,
            label: &str,
            usage: &str,
        ) -> Result<String, ()> {
            let value = args.get(*index).cloned();
            if value.is_some() {
                *index += 1;
            }
            value.ok_or_else(|| {
                eprintln!("missing value for {label}\nusage: {usage}");
            })
        }
        let mut value = |label: &str| next_value(args, &mut index, label, usage);
        match arg.as_str() {
            "--node" => match value("--node") {
                Ok(node_id) => patch.node_id = node_id,
                Err(()) => return Ok(2),
            },
            "--runtime" => match value("--runtime") {
                Ok(runtime) => patch.runtime = Some(runtime),
                Err(()) => return Ok(2),
            },
            "--profile" => match value("--profile") {
                Ok(profile) => patch.provider_profile_id = Some(profile),
                Err(()) => return Ok(2),
            },
            "--pool" => match value("--pool") {
                Ok(pool) => patch.provider_pool = Some(pool),
                Err(()) => return Ok(2),
            },
            "--model" => match value("--model") {
                Ok(model) => patch.model = Some(model),
                Err(()) => return Ok(2),
            },
            "--timeout-ms" => match value("--timeout-ms") {
                Ok(raw) => match raw.parse::<u64>() {
                    Ok(number) => patch.timeout_ms = Some(number),
                    Err(_) => {
                        eprintln!("--timeout-ms expects a number\nusage: {usage}");
                        return Ok(2);
                    }
                },
                Err(()) => return Ok(2),
            },
            "--visible" => patch.visible = Some(true),
            "--invisible" => patch.visible = Some(false),
            "--enable" => patch.enabled = Some(true),
            "--disable" => patch.enabled = Some(false),
            _ if path.is_none() => path = Some(arg),
            _ => {
                eprintln!("unknown option: {arg}\nusage: {usage}");
                return Ok(2);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: {usage}");
        return Ok(2);
    };
    if patch.node_id.is_empty() {
        eprintln!("--node is required\nusage: {usage}");
        return Ok(2);
    }
    send(
        "cli:workflow:bind",
        Method::WorkflowUpdate(WorkflowUpdateParams {
            path,
            patches: vec![patch],
        }),
    )
}

fn print_workflow_help() {
    eprintln!("herdr workflow commands:");
    eprintln!("  herdr workflow run <path.aflow.json>");
    eprintln!("  herdr workflow list");
    eprintln!("  herdr workflow get <run_id>");
    eprintln!("  herdr workflow pause <run_id>");
    eprintln!("  herdr workflow resume <run_id>");
    eprintln!("  herdr workflow cancel <run_id>");
    eprintln!("  herdr workflow delete <run_id>");
    eprintln!(
        "  herdr workflow bind <path.aflow.json> --node <id> [--runtime <r>] [--profile <id>] \
         [--pool <group>] [--model <m>] [--timeout-ms <n>] [--visible|--invisible] [--enable|--disable]"
    );
}
