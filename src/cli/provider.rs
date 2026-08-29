use crate::api::schema::{
    Method, ProviderCreateParams, ProviderDeleteParams, ProviderGetParams, ProviderListParams,
    ProviderModelEntry, ProviderModelSource, ProviderModelsFetchParams, ProviderPresetsParams,
    ProviderProtocol, ProviderRevealParams, ProviderTestParams, ProviderUpdateParams, Request,
};

pub(super) fn run_provider_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_provider_help();
        return Ok(2);
    };

    match subcommand {
        "list" => provider_list(&args[1..]),
        "get" => provider_get(&args[1..]),
        "create" => provider_create(&args[1..]),
        "update" => provider_update(&args[1..]),
        "delete" => provider_delete(&args[1..]),
        "presets" => provider_presets(&args[1..]),
        "test" => provider_test(&args[1..]),
        "models" => provider_models_fetch(&args[1..]),
        "reveal" => provider_reveal(&args[1..]),
        "help" | "--help" | "-h" => {
            print_provider_help();
            Ok(0)
        }
        _ => {
            print_provider_help();
            Ok(2)
        }
    }
}

fn parse_protocol(raw: &str) -> Result<ProviderProtocol, String> {
    match raw {
        "openai-compat" => Ok(ProviderProtocol::OpenaiCompat),
        "anthropic" => Ok(ProviderProtocol::Anthropic),
        "gemini" => Ok(ProviderProtocol::Gemini),
        other => Err(format!(
            "unknown protocol: {other} (expected openai-compat, anthropic, or gemini)"
        )),
    }
}

fn send_provider_request(id: &'static str, method: Method) -> std::io::Result<i32> {
    super::print_response(&super::send_request(&Request {
        id: id.into(),
        method,
    })?)
}

fn provider_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("unknown option: {}", args[0]);
        return Ok(2);
    }
    send_provider_request(
        "cli:provider:list",
        Method::ProviderList(ProviderListParams::default()),
    )
}

fn provider_presets(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("unknown option: {}", args[0]);
        return Ok(2);
    }
    send_provider_request(
        "cli:provider:presets",
        Method::ProviderPresets(ProviderPresetsParams::default()),
    )
}

fn require_single_profile_id(args: &[String], usage: &str) -> Option<String> {
    let Some(profile_id) = args.first() else {
        eprintln!("usage: {usage}");
        return None;
    };
    if args.len() != 1 {
        eprintln!("usage: {usage}");
        return None;
    }
    Some(profile_id.clone())
}

fn provider_get(args: &[String]) -> std::io::Result<i32> {
    let Some(profile_id) = require_single_profile_id(args, "herdr provider get <profile_id>")
    else {
        return Ok(2);
    };
    send_provider_request(
        "cli:provider:get",
        Method::ProviderGet(ProviderGetParams { profile_id }),
    )
}

fn provider_create(args: &[String]) -> std::io::Result<i32> {
    let mut name: Option<String> = None;
    let mut preset_id: Option<String> = None;
    let mut protocol: Option<ProviderProtocol> = None;
    let mut base_url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut note: Option<String> = None;
    let mut models: Vec<ProviderModelEntry> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--name" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --name");
                    return Ok(2);
                };
                name = Some(value.clone());
                index += 2;
            }
            "--preset" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --preset");
                    return Ok(2);
                };
                preset_id = Some(value.clone());
                index += 2;
            }
            "--protocol" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --protocol");
                    return Ok(2);
                };
                protocol = Some(match parse_protocol(value) {
                    Ok(protocol) => protocol,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                });
                index += 2;
            }
            "--base-url" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --base-url");
                    return Ok(2);
                };
                base_url = Some(value.clone());
                index += 2;
            }
            "--key" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --key");
                    return Ok(2);
                };
                api_key = Some(value.clone());
                index += 2;
            }
            "--note" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --note");
                    return Ok(2);
                };
                note = Some(value.clone());
                index += 2;
            }
            "--model" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --model");
                    return Ok(2);
                };
                models.push(ProviderModelEntry {
                    id: value.clone(),
                    visible: true,
                    source: ProviderModelSource::Manual,
                });
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(name) = name else {
        eprintln!("missing required option: --name");
        return Ok(2);
    };
    let Some(base_url) = base_url else {
        eprintln!("missing required option: --base-url");
        return Ok(2);
    };
    let Some(protocol) = protocol else {
        eprintln!("missing required option: --protocol");
        return Ok(2);
    };

    send_provider_request(
        "cli:provider:create",
        Method::ProviderCreate(ProviderCreateParams {
            name,
            preset_id,
            protocol,
            base_url,
            api_key,
            models,
            note,
        }),
    )
}

fn provider_update(args: &[String]) -> std::io::Result<i32> {
    let Some(profile_id) = args.first().cloned() else {
        eprintln!("usage: herdr provider update <profile_id> [options]");
        return Ok(2);
    };

    let mut name: Option<String> = None;
    let mut protocol: Option<ProviderProtocol> = None;
    let mut base_url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut weight: Option<u32> = None;
    let mut is_disabled: Option<bool> = None;
    let mut note: Option<String> = None;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--name" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --name");
                    return Ok(2);
                };
                name = Some(value.clone());
                index += 2;
            }
            "--protocol" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --protocol");
                    return Ok(2);
                };
                protocol = Some(match parse_protocol(value) {
                    Ok(protocol) => protocol,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                });
                index += 2;
            }
            "--base-url" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --base-url");
                    return Ok(2);
                };
                base_url = Some(value.clone());
                index += 2;
            }
            "--key" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --key");
                    return Ok(2);
                };
                api_key = Some(value.clone());
                index += 2;
            }
            "--weight" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --weight");
                    return Ok(2);
                };
                let Ok(parsed) = value.parse::<u32>() else {
                    eprintln!("invalid value for --weight: {value}");
                    return Ok(2);
                };
                weight = Some(parsed);
                index += 2;
            }
            "--disable" => {
                is_disabled = Some(true);
                index += 1;
            }
            "--enable" => {
                is_disabled = Some(false);
                index += 1;
            }
            "--note" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --note");
                    return Ok(2);
                };
                note = Some(value.clone());
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    send_provider_request(
        "cli:provider:update",
        Method::ProviderUpdate(ProviderUpdateParams {
            profile_id,
            name,
            protocol,
            base_url,
            api_key,
            weight,
            is_disabled,
            note,
        }),
    )
}

fn provider_delete(args: &[String]) -> std::io::Result<i32> {
    let Some(profile_id) = require_single_profile_id(args, "herdr provider delete <profile_id>")
    else {
        return Ok(2);
    };
    send_provider_request(
        "cli:provider:delete",
        Method::ProviderDelete(ProviderDeleteParams { profile_id }),
    )
}

fn provider_test(args: &[String]) -> std::io::Result<i32> {
    let Some(profile_id) = require_single_profile_id(args, "herdr provider test <profile_id>")
    else {
        return Ok(2);
    };
    send_provider_request(
        "cli:provider:test",
        Method::ProviderTest(ProviderTestParams { profile_id }),
    )
}

fn provider_models_fetch(args: &[String]) -> std::io::Result<i32> {
    let Some(profile_id) = require_single_profile_id(args, "herdr provider models <profile_id>")
    else {
        return Ok(2);
    };
    send_provider_request(
        "cli:provider:models",
        Method::ProviderModelsFetch(ProviderModelsFetchParams { profile_id }),
    )
}

fn provider_reveal(args: &[String]) -> std::io::Result<i32> {
    let Some(profile_id) = require_single_profile_id(args, "herdr provider reveal <profile_id>")
    else {
        return Ok(2);
    };
    send_provider_request(
        "cli:provider:reveal",
        Method::ProviderReveal(ProviderRevealParams { profile_id }),
    )
}

fn print_provider_help() {
    eprintln!("herdr provider commands:");
    eprintln!("  herdr provider list");
    eprintln!("  herdr provider presets");
    eprintln!("  herdr provider get <profile_id>");
    eprintln!(
        "  herdr provider create --name NAME --protocol openai-compat|anthropic|gemini --base-url URL [--preset PRESET_ID] [--key KEY] [--model MODEL_ID]... [--note TEXT]"
    );
    eprintln!(
        "  herdr provider update <profile_id> [--name NAME] [--protocol ...] [--base-url URL] [--key KEY] [--weight N] [--disable|--enable] [--note TEXT]"
    );
    eprintln!("  herdr provider delete <profile_id>");
    eprintln!("  herdr provider test <profile_id>");
    eprintln!("  herdr provider models <profile_id>");
    eprintln!("  herdr provider reveal <profile_id>");
}
