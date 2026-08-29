use crate::api::schema::{
    Method, Request, WorkflowCancelParams, WorkflowDeleteParams, WorkflowGetParams,
    WorkflowListParams, WorkflowPauseParams, WorkflowResumeParams, WorkflowRunParams,
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

fn print_workflow_help() {
    eprintln!("herdr workflow commands:");
    eprintln!("  herdr workflow run <path.aflow.json>");
    eprintln!("  herdr workflow list");
    eprintln!("  herdr workflow get <run_id>");
    eprintln!("  herdr workflow pause <run_id>");
    eprintln!("  herdr workflow resume <run_id>");
    eprintln!("  herdr workflow cancel <run_id>");
    eprintln!("  herdr workflow delete <run_id>");
}
