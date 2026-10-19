//! Every ```toml and ```json block in the README must parse.

const README: &str = include_str!("../../../README.md");

fn blocks(lang: &str) -> Vec<&'static str> {
    let fence = format!("```{lang}\n");
    README
        .split(fence.as_str())
        .skip(1)
        .map(|rest| &rest[..rest.find("```").expect("unterminated code block")])
        .collect()
}

#[test]
fn readme_toml_blocks_parse() {
    let blocks = blocks("toml");
    assert!(!blocks.is_empty());
    for block in blocks {
        if let Err(e) = block.parse::<toml::Table>() {
            panic!("README TOML block does not parse: {e}\n{block}");
        }
    }
}

#[test]
fn readme_json_blocks_parse() {
    let blocks = blocks("json");
    assert!(!blocks.is_empty());
    for block in blocks {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(block) {
            panic!("README JSON block does not parse: {e}\n{block}");
        }
    }
}
