use std::collections::HashMap;

pub fn eval(expr: &str, bindings: &HashMap<String, String>) -> Result<serde_json::Value, String> {
    use jaq_core::{Compiler, Ctx, Vars, data};
    use jaq_core::load::{Arena, File, Loader};
    use jaq_json::Val;

    let program = File { code: expr, path: () };
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).map_err(|errors| {
        let msgs: Vec<String> = errors.into_iter().map(|e| format!("{e:?}")).collect();
        format!("jq parse error: {}", msgs.join("; "))
    })?;

    let var_names: Vec<String> = bindings.keys().map(|k| format!("${k}")).collect();
    let var_refs: Vec<&str> = var_names.iter().map(|s| s.as_str()).collect();
    let filter = Compiler::default()
        .with_funs(funs)
        .with_global_vars(var_refs.iter().copied())
        .compile(modules)
        .map_err(|errors| {
            let msgs: Vec<String> = errors.into_iter().map(|e| format!("{e:?}")).collect();
            format!("jq compile error: {}", msgs.join("; "))
        })?;

    let keys_ordered: Vec<&String> = bindings.keys().collect();
    let var_vals: Vec<Val> = keys_ordered.iter().map(|k| Val::from(bindings[*k].clone())).collect();
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new(var_vals));
    let input = Val::Null;

    let mut results = Vec::new();
    for item in filter.id.run((ctx, input)) {
        match item {
            Ok(val) => results.push(val),
            Err(err) => return Err(format!("jq runtime error: {err:?}")),
        }
    }

    if results.is_empty() {
        return Err("jq expression produced no output".to_string());
    }
    if results.len() > 1 {
        return Err(format!("jq expression produced {} outputs; exactly one required", results.len()));
    }

    let rendered = results[0].to_string();
    serde_json::from_str(&rendered).map_err(|e| format!("jq output is not valid JSON: {e}"))
}
