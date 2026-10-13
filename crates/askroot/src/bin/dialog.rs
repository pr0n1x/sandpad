//! The dialog: the prompt on stdin, `approve` or `deny` on stdout. Run by
//! the `sandbox` process supervising a `-p root` sandbox, once per request.
//! Closing the window denies.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

use iced::widget::{button, column, container, row, text};
use iced::{window, Element, Length, Size, Task};
use sandbox_askroot::Prompt;

fn main() {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("sandbox-askroot-dialog: cannot read the prompt: {e}");
        println!("deny");
        std::process::exit(1);
    }
    let approved = ask(Prompt::parse(&text));
    println!("{}", if approved { "approve" } else { "deny" });
}

/// The answer outlives the runtime: stored where `ask` reads it back.
static APPROVED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy)]
enum Message {
    Answer(bool),
}

/// Show the window; true when approved. Must run on the main thread.
fn ask(prompt: Prompt) -> bool {
    let result = iced::application(move || prompt.clone(), update, view)
        .title("Sandbox root request")
        .window(window::Settings {
            size: Size::new(560.0, 300.0),
            position: window::Position::Centered,
            resizable: false,
            level: window::Level::AlwaysOnTop,
            ..window::Settings::default()
        })
        .run();
    if let Err(e) = result {
        eprintln!("sandbox-askroot-dialog: {e}");
        return false;
    }
    APPROVED.load(Ordering::SeqCst)
}

fn update(_: &mut Prompt, message: Message) -> Task<Message> {
    match message {
        Message::Answer(approve) => {
            APPROVED.store(approve, Ordering::SeqCst);
            iced::exit()
        }
    }
}

fn view(p: &Prompt) -> Element<'_, Message> {
    let heading = format!(
        "{} in sandbox \"{}\" asks for root inside the sandbox.",
        p.app, p.box_name
    );
    let detail = column![
        row![text("Command: ").size(14), text(&p.command).size(14)],
        row![text("Directory: ").size(14), text(&p.cwd).size(14)],
        row![text("User: ").size(14), text(&p.user).size(14)],
    ]
    .spacing(4);
    let note = text(
        "Root here means write access to the sandbox's own system layer \
         (its usr, etc, opt, var). The host is not affected.",
    )
    .size(13);
    let buttons = row![
        button(text("Approve sandboxed root access")).on_press(Message::Answer(true)),
        button(text("Deny")).on_press(Message::Answer(false)),
    ]
    .spacing(16);
    container(
        column![text(heading).size(16), detail, note, buttons]
            .spacing(16)
            .padding(20),
    )
    .center_x(Length::Fill)
    .into()
}
