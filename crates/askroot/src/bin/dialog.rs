//! The dialog: the prompt on stdin, `approve` or `deny` on stdout. Run by
//! the `sandpad` process supervising a `-p root` sandbox, once per request.
//! Closing the window denies.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

use iced::keyboard::{self, key::Named, Key};
use iced::widget::{button, column, container, row, text};
use iced::{window, Color, Element, Length, Size, Subscription, Task, Theme};
use sandpad_askroot::Prompt;

fn main() {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("sandpad-askroot-dialog: cannot read the prompt: {e}");
        println!("deny");
        std::process::exit(1);
    }
    let approved = ask(Prompt::parse(&text));
    println!("{}", if approved { "approve" } else { "deny" });
}

/// The answer outlives the runtime: stored where `ask` reads it back.
static APPROVED: AtomicBool = AtomicBool::new(false);

struct Dialog {
    prompt: Prompt,
    focused: Choice,
}

impl Dialog {
    fn new(prompt: Prompt) -> Self {
        Self {
            prompt,
            focused: Choice::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
    Answer(bool),
    SwitchFocus,
    Activate,
}

/// Show the window; true when approved. Must run on the main thread.
fn ask(prompt: Prompt) -> bool {
    let result = iced::application(move || Dialog::new(prompt.clone()), update, view)
        .title("Sandbox root request")
        .subscription(|_| subscription())
        .window(window::Settings {
            size: Size::new(560.0, 300.0),
            position: window::Position::Centered,
            resizable: false,
            level: window::Level::AlwaysOnTop,
            ..window::Settings::default()
        })
        .run();
    if let Err(e) = result {
        eprintln!("sandpad-askroot-dialog: {e}");
        return false;
    }
    APPROVED.load(Ordering::SeqCst)
}

fn subscription() -> Subscription<Message> {
    keyboard::listen().filter_map(keyboard_message)
}

fn keyboard_message(event: keyboard::Event) -> Option<Message> {
    let keyboard::Event::KeyPressed {
        key,
        modifiers,
        repeat: false,
        ..
    } = event
    else {
        return None;
    };
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    match key.as_ref() {
        Key::Named(
            Named::ArrowLeft | Named::ArrowRight | Named::ArrowUp | Named::ArrowDown | Named::Tab,
        ) => Some(Message::SwitchFocus),
        Key::Named(Named::Enter | Named::Space) | Key::Character(" ") => Some(Message::Activate),
        Key::Named(Named::Escape) => Some(Message::Answer(false)),
        _ => None,
    }
}

fn update(state: &mut Dialog, message: Message) -> Task<Message> {
    match message {
        Message::SwitchFocus => {
            state.focused = match state.focused {
                Choice::Approve => Choice::Deny,
                Choice::Deny => Choice::Approve,
            };
            Task::none()
        }
        Message::Activate => update(state, Message::Answer(state.focused == Choice::Approve)),
        Message::Answer(approve) => {
            APPROVED.store(approve, Ordering::SeqCst);
            iced::exit()
        }
    }
}

fn button_style(theme: &Theme, status: button::Status, focused: bool) -> button::Style {
    let mut style = if focused {
        button::primary(theme, status)
    } else {
        button::secondary(theme, status)
    };
    style.border.width = 2.0;
    style.border.color = if focused {
        style.text_color
    } else {
        Color::TRANSPARENT
    };
    style
}

fn view(state: &Dialog) -> Element<'_, Message> {
    let p = &state.prompt;
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
        button(text("Approve sandboxed root access"))
            .on_press(Message::Answer(true))
            .style(move |theme, status| button_style(
                theme,
                status,
                state.focused == Choice::Approve
            )),
        button(text("Deny"))
            .on_press(Message::Answer(false))
            .style(move |theme, status| button_style(theme, status, state.focused == Choice::Deny)),
    ]
    .spacing(16);
    container(
        column![
            text(heading).size(16),
            detail,
            note,
            buttons,
            text("Arrow keys / Tab to choose; Enter / Space to activate").size(12),
        ]
        .spacing(16)
        .padding(20),
    )
    .center_x(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::NONE,
            text: None,
            repeat: false,
        }
    }

    #[test]
    fn keyboard_navigation_activates_the_highlighted_choice() {
        let mut dialog = Dialog::new(Prompt::default());
        assert_eq!(dialog.focused, Choice::Deny);
        let _ = update(
            &mut dialog,
            keyboard_message(press(Key::Named(Named::Enter))).unwrap(),
        );
        assert!(!APPROVED.load(Ordering::SeqCst));
        for key in [
            Named::ArrowLeft,
            Named::ArrowRight,
            Named::ArrowUp,
            Named::ArrowDown,
            Named::Tab,
        ] {
            let _ = update(
                &mut dialog,
                keyboard_message(press(Key::Named(key))).unwrap(),
            );
            assert_eq!(dialog.focused, Choice::Approve);
            let _ = update(
                &mut dialog,
                keyboard_message(press(Key::Named(Named::Space))).unwrap(),
            );
            assert!(APPROVED.load(Ordering::SeqCst));
            let _ = update(
                &mut dialog,
                keyboard_message(press(Key::Named(key))).unwrap(),
            );
            assert_eq!(dialog.focused, Choice::Deny);
            let _ = update(
                &mut dialog,
                keyboard_message(press(Key::Named(Named::Enter))).unwrap(),
            );
            assert!(!APPROVED.load(Ordering::SeqCst));
        }
        let _ = update(&mut dialog, Message::Answer(true));
        let _ = update(
            &mut dialog,
            keyboard_message(press(Key::Named(Named::Escape))).unwrap(),
        );
        assert!(!APPROVED.load(Ordering::SeqCst));
        let mut repeated = press(Key::Named(Named::Enter));
        if let keyboard::Event::KeyPressed { repeat, .. } = &mut repeated {
            *repeat = true;
        }
        assert_eq!(keyboard_message(repeated), None);
        assert_eq!(keyboard_message(press(Key::Character("x".into()))), None);
    }

    #[test]
    fn focus_is_distinct_in_light_and_dark_themes() {
        for theme in [Theme::Light, Theme::Dark] {
            let focused = button_style(&theme, button::Status::Active, true);
            let other = button_style(&theme, button::Status::Active, false);
            assert_ne!(focused.background, other.background);
            assert_ne!(focused.border.color, other.border.color);
            assert_ne!(focused.border.color, Color::TRANSPARENT);
            assert_eq!(focused.border.width, other.border.width);
        }
    }
}
