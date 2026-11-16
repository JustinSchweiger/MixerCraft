fn main() {
    // Dark widget style regardless of the system theme (the mixer is dark).
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("compile .slint UI");
}
