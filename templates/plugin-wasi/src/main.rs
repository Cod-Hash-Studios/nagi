use std::io::{self, Read};

fn main() {
    // Nagi sends a versioned PluginInspectorInputV1 document on stdin.
    // Read it even when this starter does not need fields yet, so a broken host
    // pipe fails during development instead of after publishing.
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("read Nagi input");
    assert!(!input.trim().is_empty(), "Nagi inspector input is required");

    println!(
        r#"{{"schema_version":1,"summary":"Plugin connected","blocks":[{{"type":"notice","tone":"success","title":"{PLUGIN_NAME}","body":"This structured panel is rendered by Nagi."}}]}}"#
    );
}

