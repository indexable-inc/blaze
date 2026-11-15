//! The native Question leaf: prompt, associated option explanations and an existing native editor. The REPL owns
//! answering and waiting; this view only presents state and sends explicit choices or current text.
use super::*;

pub(super) const MAX_OPTIONS: usize = 200;
const MAX_LABEL: usize = 200;
const MAX_PROMPT: usize = 140;
const PROPS: &[&str] = &[
    "text", "detail", "options", "recommended", "ahead", "value", "answer", "allowText", "onChoose", "onChange", "onSubmit",
];

/// Refuse malformed wire data as a whole, including unknown option fields and over-limit lists. Never silently drop
/// options: their exact labels are the answers a click sends.
pub(super) fn check(node: &Value) -> Option<String> {
    let Some(p) = node["props"].as_object() else { return Some("<Question> props must be a record".to_owned()) };
    if let Some(name) = p.keys().find(|k| !PROPS.contains(&k.as_str()) && !common::COMMON.contains(&k.as_str())) {
        return Some(format!("<Question> has no prop {name:?}"));
    }
    if !matches!(&node["children"], Value::Null | Value::Array(_)) || node["children"].as_array().is_some_and(|a| !a.is_empty()) {
        return Some("<Question> is a leaf and takes no children".to_owned());
    }
    for key in ["text", "value"] {
        if !node["props"][key].is_string() {
            return Some(format!("<Question> {key} must be words"));
        }
    }
    if words(node, "text").trim().is_empty() || words(node, "text").encode_utf16().count() > MAX_PROMPT {
        return Some(format!("<Question> text must contain 1–{MAX_PROMPT} characters"));
    }
    for key in ["detail", "ahead", "recommended", "answer", "onChoose", "onChange", "onSubmit"] {
        if !matches!(&node["props"][key], Value::Null | Value::String(_)) {
            return Some(format!("<Question> {key} must be words"));
        }
    }
    if !node["props"]["allowText"].is_boolean() {
        return Some("<Question> allowText must be true or false".to_owned());
    }
    let Some(options) = node["props"]["options"].as_array() else { return Some("<Question> options must be a list of {label, detail?} records".to_owned()) };
    if options.len() > MAX_OPTIONS {
        return Some(format!("<Question> options has {} entries, more than {MAX_OPTIONS}", options.len()));
    }
    for (i, option) in options.iter().enumerate() {
        let Some(record) = option.as_object() else { return Some(format!("<Question> options[{i}] must be a {{label, detail?}} record")) };
        if let Some(key) = record.keys().find(|k| !matches!(k.as_str(), "label" | "detail")) {
            return Some(format!("<Question> options[{i}] has no field {key:?}"));
        }
        let Some(label) = option["label"].as_str() else { return Some(format!("<Question> options[{i}].label must be words")) };
        if label.trim().is_empty() || label.encode_utf16().count() > MAX_LABEL {
            return Some(format!("<Question> options[{i}].label must contain 1–{MAX_LABEL} characters"));
        }
        if !matches!(&option["detail"], Value::Null | Value::String(_)) {
            return Some(format!("<Question> options[{i}].detail must be words"));
        }
        if options[..i].iter().any(|previous| previous["label"] == option["label"]) {
            return Some(format!("<Question> options[{i}].label {label:?} is duplicated"));
        }
    }
    for key in ["recommended", "ahead"] {
        if let Some(label) = prop(node, key).as_str()
            && !options.iter().any(|option| option["label"].as_str() == Some(label))
        {
            return Some(format!("<Question> {key} {label:?} is not an option label"));
        }
    }
    if options.is_empty() && prop(node, "allowText") == &Value::Bool(false) && prop(node, "answer").as_str().is_none_or(str::is_empty) {
        return Some("<Question> needs options or allowText to accept an answer".to_owned());
    }
    None
}

pub(super) fn ahead_text(node: &Value) -> String {
    let ahead = words(node, "ahead");
    if ahead.is_empty() { String::new() } else { format!("Proceeded with {ahead} · choose another to change it") }
}

fn optional_words(v: View<'_>, text: &str) {
    v.text(text);
    let mut size = v.entity().try_cloned::<&Size>().unwrap_or_default();
    size.h = if text.is_empty() { Sizing::fixed(0.0) } else { Sizing::FIT };
    if v.entity().try_cloned::<&Size>() != Some(size) { v.set(size); }
}

pub(super) fn element(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
    let answer = prop(node, "answer").as_str().filter(|s| !s.is_empty());
    let options = prop(node, "options").as_array().expect("Question options were checked");
    let recommended = prop(node, "recommended").as_str().filter(|s| !s.trim().is_empty());
    let allow_text = prop(node, "allowText") == &Value::Bool(true);
    let mut parts = Vec::with_capacity(5);
    if answer.is_some() {
        parts.push(("receipt", "hn.GenQuestionReceipt"));
    } else {
        parts.push(("heading", "hn.GenQuestionHeading"));
        if !options.is_empty() { parts.push(("options", "hn.GenQuestionOptions")); }
        if allow_text { parts.push(("response", "hn.GenQuestionResponse")); }
        if allow_text || recommended.is_some() { parts.push(("actions", "hn.GenQuestionActions")); }
        if !words(node, "ahead").is_empty() { parts.push(("ahead", "hn.GenQuestionDetail")); }
    }
    view::for_each_of(v, parts.into_iter().map(|(k, p)| (k, p, ())), |_, _, _| {});
    if let Some(receipt) = v.part("receipt") {
        if let Some(t) = receipt.part("question") { t.text(&words(node, "text")); }
        if let Some(t) = receipt.part("answer") { t.text(answer.expect("answered receipt")); }
        return;
    }
    if let Some(heading) = v.part("heading") {
        if let Some(t) = heading.part("title") { t.text(&words(node, "text")); }
        if let Some(t) = heading.part("detail") { optional_words(t, &words(node, "detail")); }
    }
    if let Some(list) = v.part("options") {
        let h = control::handler(node, "onChoose");
        view::for_each_of(list, options.iter().enumerate().map(|(i, o)| {
            let recommended = o["label"].as_str() == recommended;
            (format!("o{i}"), if recommended { "hn.GenQuestionOptionRecommended" } else { "hn.GenQuestionOption" }, o)
        }), |row, option, _| {
            let label = option["label"].as_str().expect("checked option label");
            let head = row.part("head").expect("Question option heading");
            if let Some(t) = head.part("label") { t.text(label); }
            if let Some(t) = row.part("detail") { optional_words(t, option["detail"].as_str().unwrap_or("")); }
            if let Some(t) = head.part("recommendation") { optional_words(t, if Some(label) == recommended { "Recommended" } else { "" }); }
            control::arm(row, h.as_deref().map(|h| control::send(b.cell, h, Value::String(label.to_owned()))).as_deref());
        });
    }
    let (submit, enabled, has_text) = if let Some(response) = v.part("response") {
        let (action, enabled, has_text) = control::question_field(response, node, b);
        (Some(action), enabled, has_text)
    } else {
        let action = control::handler(node, "onSubmit").zip(recommended).map(|(h, value)| control::send(b.cell, &h, Value::String(value.to_owned())));
        let enabled = action.is_some();
        (action, enabled, false)
    };
    if let Some(actions) = v.part("actions") {
        if let Some(button) = actions.part("submit") {
            if let Some(label) = button.part("label") { label.text(if has_text || recommended.is_none() { "Send answer" } else { "Use recommendation" }); }
            control::arm(button, submit.as_deref().filter(|_| enabled));
            control::inert(button, !enabled);
        }
        if let Some(hint) = actions.part("hint") {
            optional_words(hint, if !allow_text { "" } else if recommended.is_some() && !has_text { "Choose an option or write your own" } else { "Return to send" });
        }
    }
    if let Some(ahead) = v.part("ahead") { ahead.text(&ahead_text(node)); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_refuses_lossy_or_ambiguous_wire_data() {
        let base = serde_json::json!({"type":"Question", "props": {
            "text":"Which environment?", "value":"", "allowText":true,
            "options":[{"label":"Staging", "detail":"No customer traffic"}]
        }, "children":[]});
        assert_eq!(check(&base), None);
        for (path, value, why) in [
            ("/props/options", serde_json::json!(["Staging"]), "record"),
            ("/props/options", serde_json::json!([{"label":"Staging","value":"s"}]), "value"),
            ("/props/options", serde_json::json!([{"label":"a"},{"label":"a"}]), "duplicated"),
            ("/props/options", serde_json::json!([{"label":"x".repeat(201)}]), "200"),
            ("/props/options", serde_json::json!((0..201).map(|i| serde_json::json!({"label":i.to_string()})).collect::<Vec<_>>()), "200"),
            ("/children", serde_json::json!([{"type":"Text"}]), "leaf"),
            ("/props/allowText", serde_json::json!("yes"), "allowText"),
        ] {
            let mut node = base.clone();
            *node.pointer_mut(path).expect("test path") = value;
            assert!(check(&node).is_some_and(|s| s.contains(why)), "{node}");
        }
        let mut unknown = base;
        unknown["props"]["default"] = serde_json::json!("Staging");
        assert!(check(&unknown).is_some_and(|s| s.contains("default")));
    }

    /// Prompt and option explanations wrap within narrow and wide rooms; long option sets stay bounded. A native
    /// pointer press activates exactly the label it shows, and the answered state removes the entire input surface.
    #[test]
    fn question_layout_and_native_choice_dispatch() {
        use harness_native_api::Clock;
        use harness_native_ui_input_api::{Activated, PointerState};
        use harness_native_ui_layout_api::{ScrollExtent, Viewport};
        for width in [280.0, 720.0] {
            let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
            let w = world.world_ref();
            harness_native_ui_layout::install(w);
            harness_native_ui_input::install(w);
            harness_native_ui_motion::install(w);
            world.set(Clock::default());
            world.set(Viewport { w: width, h: 1600.0 });
            let row = View::root(w, "hn.ChatUi");
            let options: Vec<Value> = (0..40).map(|i| serde_json::json!({
                "label": format!("Environment {i}"),
                "detail": "Run the change here before sending traffic to customers."
            })).collect();
            let mut node = serde_json::json!({"type":"Question", "props": {
                "text":"Which environment should receive the next version of the application?",
                "detail":"Choose an environment below, or write a specific deployment target.",
                "options":options, "recommended":"Environment 0", "allowText":true, "value":"",
                "onChoose":"choose", "onChange":"change", "onSubmit":"submit"
            }, "children":[]});
            let mut measure = |s: &str, style: &TextStyle, _md: bool| s.chars().count() as f32 * style.size.max(1.0) * 0.55;
            let mut frame = |node: &Value, pointer: Option<PointerState>| {
                if let Some(pointer) = pointer { world.set(pointer); }
                fill(row, node, "test.question", None, false, &mut measure);
                harness_native_ui_layout::typeset::typeset(w, &mut harness_native_ui_layout::typeset::Estimate);
                world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
                world.progress_time(1.0 / 60.0);
            };
            for _ in 0..4 { frame(&node, None); }
            let question = row.part("body").unwrap().part("g0").unwrap();
            let laid = |v: View<'_>| v.entity().try_cloned::<&Laid>().expect("laid out question part");
            let title = question.part("heading").unwrap().part("title").unwrap();
            let list = question.part("options").unwrap();
            let option = list.part("o0").unwrap();
            let label = option.part("head").unwrap().part("label").unwrap();
            let detail = option.part("detail").unwrap();
            assert!(laid(title).w <= width && laid(title).h >= 26.0);
            assert!((laid(option).w - laid(list).w).abs() < 1.0, "options fill the available width");
            assert!(laid(detail).y >= laid(label).y + laid(label).h, "each explanation remains under its own label");
            assert!(laid(list).h <= 320.0 && laid(list).h > 0.0);
            assert!(list.entity().try_cloned::<&ScrollExtent>().is_some_and(|x| x.h > laid(list).h), "long choices scroll");
            let field = question.part("response").unwrap().part("body").unwrap();
            assert!((laid(field).w - laid(question).w).abs() < 1.0, "the answer field fills the question");
            assert!(field.part("text").unwrap().entity().has(TextEdit::id()), "the shared native editor owns its caret");
            let expected = control::send("test.question", "choose", serde_json::json!("Environment 0"));
            assert_eq!(option.entity().try_cloned::<&Action>().unwrap().name.as_str(), expected);
            let at = laid(option);
            let (x, y) = (at.x + at.w / 2.0, at.y + at.h / 2.0);
            for (down, went_down, went_up) in [(false,false,false),(true,true,false),(false,false,true)] {
                frame(&node, Some(PointerState { x, y, down, went_down, went_up }));
            }
            let mut activated = Vec::new();
            world.new_query::<&Activated>().each_entity(|e, _| {
                activated.push(e.try_cloned::<&Action>().expect("an activated action").name.as_str().to_owned());
            });
            assert_eq!(activated, [expected]);
            node["props"]["answer"] = serde_json::json!("Environment 0");
            frame(&node, None);
            let receipt = question.part("receipt").expect("compact answered receipt");
            assert!(question.part("options").is_none() && question.part("response").is_none() && question.part("actions").is_none());
            assert_eq!(receipt.part("answer").unwrap().entity().try_cloned::<&Text>().unwrap().s.as_str(), "Environment 0");
            assert!(laid(receipt).h < 120.0);
        }
    }

}
