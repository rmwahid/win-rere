//! The single self-contained HTML document that draws the widget.

const TEMPLATE: &str = include_str!("../ui/index.html");
const CSS: &str = include_str!("../ui/app.css");
const JS: &str = include_str!("../ui/app.js");

pub fn html(sidebar_visible: bool) -> String {
    let sidebar = if sidebar_visible { "visible" } else { "hidden" };
    TEMPLATE
        .replace(
            "data-sidebar=\"visible\"",
            &format!("data-sidebar=\"{sidebar}\""),
        )
        .replace("/* @@CSS@@ */", CSS)
        .replace("/* @@JS@@ */", JS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_filled_in() {
        let output = html(true);
        assert!(!output.contains("@@CSS@@"));
        assert!(!output.contains("@@JS@@"));
        assert!(output.contains("data-sidebar=\"visible\""));
        assert!(output.contains("window.__winRere"));
        assert!(output.contains("win-rere-content.example"));
        assert!(output.contains("id=\"mobile-btn\""));
        assert!(output.contains("mobile.html"));
        assert!(output.contains("has_mobile"));
    }

    #[test]
    fn the_sidebar_can_start_hidden() {
        let output = html(false);
        assert!(output.contains("data-sidebar=\"hidden\""));
        assert!(!output.contains("data-sidebar=\"visible\""));
    }
}
