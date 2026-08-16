//! Regression guard: the shipped config template must always parse and
//! pass `validate_config` cleanly. Catches template rot (stale keys,
//! invalid values) at CI time instead of at a user's first run.

#[test]
fn config_example_template_validates_clean() {
    let template = include_str!("../../../config.example.toml");
    let config = jyc_types::load_config_from_str(template)
        .expect("config.example.toml must parse and deserialize");
    let errors = jyc_types::validation::validate_config(&config);
    assert!(
        errors.is_empty(),
        "config.example.toml failed validation:\n{}",
        errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
