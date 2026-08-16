//! Regression guard: the shipped config template (`config.example.toml`,
//! embedded via `include_str!` by `jyc config init` and the auto-create
//! path) must pass `validate_config` — a template that fails its own
//! validator breaks first-run `jyc serve` for new users.

#[test]
fn config_example_template_validates() {
    let template = include_str!("../../../config.example.toml");
    let config =
        jyc_types::load_config_from_str(template).expect("config.example.toml must parse and load");
    let errors = jyc_types::validation::validate_config(&config);
    assert!(
        errors.is_empty(),
        "config.example.toml must validate cleanly, got: {errors:?}"
    );
}
