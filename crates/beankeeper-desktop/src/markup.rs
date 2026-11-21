//! HTML fragments with escaped dynamic text and compile-time template structure.

use std::fmt::{self, Display, Write};

use axum::response::Html;

#[derive(Default)]
pub(crate) struct Markup(String);

impl Markup {
    pub(crate) fn escaped(value: &str) -> Self {
        let mut markup = Self(String::with_capacity(value.len()));
        markup.push_text(value);
        markup
    }

    pub(crate) fn push_text(&mut self, value: &str) {
        Escaper(&mut self.0).escape(value);
    }

    pub(crate) fn append(&mut self, other: &Self) {
        self.0.push_str(&other.0);
    }

    pub(crate) fn write<const N: usize>(&mut self, template: &Template<N>, parts: [&dyn Part; N]) {
        let mut start = 0;
        for (slot, part) in template.slots.into_iter().zip(parts) {
            self.0.push_str(&template.structure[start..slot]);
            part.render(self);
            start = slot + 2;
        }
        self.0.push_str(&template.structure[start..]);
    }

    pub(crate) fn into_html(self) -> Html<String> {
        Html(self.0)
    }

    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Checked in a const context by `markup!` and `write_markup!`.
pub(crate) struct Template<const N: usize> {
    structure: &'static str,
    slots: [usize; N],
}

impl<const N: usize> Template<N> {
    pub(crate) const fn new(structure: &'static str) -> Self {
        let bytes = structure.as_bytes();
        let mut slots = [0; N];
        let mut found = 0;
        let mut index = 0;
        while index + 1 < bytes.len() {
            if bytes[index] == b'{' && bytes[index + 1] == b'}' {
                assert!(found < N, "more HTML slots than arguments");
                slots[found] = index;
                found += 1;
                index += 1;
            }
            index += 1;
        }
        assert!(found == N, "fewer HTML slots than arguments");
        Self { structure, slots }
    }
}

/// Interpolations render as text, with `Markup` the one structured exception.
pub(crate) trait Part {
    fn render(&self, markup: &mut Markup);
}

impl<T: Display> Part for T {
    fn render(&self, markup: &mut Markup) {
        let _ = write!(Escaper(&mut markup.0), "{self}");
    }
}

/// Conditional attributes emit only fixed HTML syntax.
pub(crate) enum Attribute {
    Selected(bool),
    CurrentPage(bool),
}

impl Part for Attribute {
    fn render(&self, markup: &mut Markup) {
        let attribute = match self {
            Self::Selected(true) => " selected",
            Self::Selected(false) => "",
            Self::CurrentPage(true) => "aria-current=\"page\"",
            Self::CurrentPage(false) => "data-view",
        };
        markup.0.push_str(attribute);
    }
}

impl Part for Markup {
    fn render(&self, markup: &mut Markup) {
        markup.append(self);
    }
}

impl Part for &Markup {
    fn render(&self, markup: &mut Markup) {
        markup.append(self);
    }
}

struct Escaper<'a>(&'a mut String);

impl Escaper<'_> {
    fn escape(&mut self, value: &str) {
        let mut start = 0;
        for (index, byte) in value.bytes().enumerate() {
            let entity = match byte {
                b'&' => "&amp;",
                b'<' => "&lt;",
                b'>' => "&gt;",
                b'"' => "&quot;",
                b'\'' => "&#39;",
                _ => continue,
            };
            // The matched ASCII byte is always a UTF-8 character boundary.
            self.0.push_str(&value[start..index]);
            self.0.push_str(entity);
            start = index + 1;
        }
        self.0.push_str(&value[start..]);
    }
}

impl Write for Escaper<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.escape(value);
        Ok(())
    }
}

/// Append compile-time HTML structure with `{}` for text or markup parts.
macro_rules! write_markup {
    ($output:expr, $structure:expr $(, $part:expr)* $(,)?) => {
        $output.write(
            &const { $crate::markup::Template::new($structure) },
            [$(&$part as &dyn $crate::markup::Part),*],
        )
    };
}
pub(crate) use write_markup;

/// Build an HTML fragment using the same rules as `write_markup!`.
macro_rules! markup {
    ($structure:expr $(, $part:expr)* $(,)?) => {{
        let mut fragment = $crate::markup::Markup::default();
        $crate::markup::write_markup!(fragment, $structure $(, $part)*);
        fragment
    }};
}
pub(crate) use markup;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_structure_and_escapes_all_dynamic_parts() {
        let label = "Café & \"bread\" <today's>";
        let nested = markup!("<span>{}</span>", label);
        let html = markup!("<p title=\"{}\">{}</p>", label, nested);
        assert_eq!(
            html.as_str(),
            "<p title=\"Café &amp; &quot;bread&quot; &lt;today&#39;s&gt;\"><span>Café &amp; &quot;bread&quot; &lt;today&#39;s&gt;</span></p>"
        );
        assert_eq!(
            Markup::escaped(label).as_str(),
            "Café &amp; &quot;bread&quot; &lt;today&#39;s&gt;"
        );
    }
}
