mod dioxus_gui; // dioxus implementation
mod lang; mod audio; mod server; mod client; mod buffers; mod net; mod types;
use anyhow::Result;

fn main() -> Result<()> {
    lang::init_lang("zh");
    dioxus_gui::run()?;
    Ok(())
}
