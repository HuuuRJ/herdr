//! `{{node_id.output}}` template rendering.

/// Render `{{id.output}}` references against the completed outputs map.
/// Unknown references are a validation-time error (`model::validate`
/// guarantees they are upstream); rendering still fails loudly rather than
/// silently substituting empty text.
pub(crate) fn render(
    template: &str,
    outputs: &std::collections::HashMap<String, String>,
) -> Result<String, String> {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after_braces = &rest[start + 2..];
        let Some(end) = after_braces.find("}}") else {
            break;
        };
        let token = &after_braces[..end];
        if let Some(id) = token.strip_suffix(".output") {
            let Some(value) = outputs.get(id) else {
                return Err(format!("output of node '{id}' is not available yet"));
            };
            result.push_str(&rest[..start]);
            result.push_str(value);
        } else {
            // Not an output reference: keep the literal token.
            result.push_str(&rest[..start + 2]);
            result.push_str(token);
            result.push_str("}}");
        }
        rest = &after_braces[end + 2..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_known_outputs() {
        let map = outputs(&[("a", "first"), ("b", "second")]);
        assert_eq!(
            render("x {{a.output}} mid {{b.output}} y", &map).unwrap(),
            "x first mid second y"
        );
    }

    #[test]
    fn repeated_references_and_plain_text_pass_through() {
        let map = outputs(&[("a", "v")]);
        assert_eq!(
            render("{{a.output}}{{a.output}} {{not.a.ref}} {z}", &map).unwrap(),
            "vv {{not.a.ref}} {z}"
        );
    }

    #[test]
    fn missing_output_is_an_error() {
        let err = render("{{ghost.output}}", &outputs(&[])).unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn multiline_outputs_are_inlined_verbatim() {
        let map = outputs(&[("a", "line1\nline2\n")]);
        assert_eq!(render("[{{a.output}}]", &map).unwrap(), "[line1\nline2\n]");
    }
}
