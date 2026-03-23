wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
});

struct EchoPlugin;

impl Guest for EchoPlugin {
    fn invoke(tool_name: String, input_json: String) -> Result<String, String> {
        if tool_name != "plugin_echo" {
            return Err(format!("unsupported tool '{tool_name}'"));
        }

        let input = serde_json::from_str::<serde_json::Value>(&input_json)
            .map_err(|error| format!("invalid input json: {error}"))?;

        let text = input
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "missing required string field 'text'".to_string())?;

        Ok(text.to_string())
    }
}

export!(EchoPlugin);
