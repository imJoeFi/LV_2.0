use iced::widget::{column, container, text};
use iced::{window, Element, Length, Size, Task, Theme};

#[derive(Default)]
struct ManagerApp;

#[derive(Debug, Clone, Copy)]
enum Message {}

fn update(_app: &mut ManagerApp, message: Message) -> Task<Message> {
    match message {}
}

fn view(_app: &ManagerApp) -> Element<'_, Message> {
    container(
        column![
            text("LightningVEND Manager").size(36),
            text("No kiosks paired").size(20),
        ]
        .spacing(16),
    )
    .padding(32)
    .center(Length::Fill)
    .into()
}

fn main() -> iced::Result {
    iced::application(ManagerApp::default, update, view)
        .theme(Theme::Dark)
        .title("LightningVEND Manager")
        .window(window::Settings {
            size: Size::new(1200.0, 800.0),
            ..window::Settings::default()
        })
        .run()
}
