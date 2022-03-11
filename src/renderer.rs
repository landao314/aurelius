use std::error::Error;
use pulldown_cmark::{html, Parser};
use std::io::{self, Write};

pub enum RenderError {}

pub trait Renderer {
    type Error: Error + Send + Sync + 'static;

    fn render(&self, source: &str, output: impl Write) -> Result<(), Self::Error>;
}

pub struct MarkdownRenderer;

impl Renderer for MarkdownRenderer {
    type Error = io::Error;

    fn render(&self, source: &str, output: impl Write) -> io::Result<()> {
        let parser = Parser::new(source);
        html::write_html(output, parser)?;
        Ok(())
    }
}
