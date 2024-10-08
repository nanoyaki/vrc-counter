#![feature(let_chains)]
// Prevents the terminal from opening on a release build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod logger;

use futures::{channel::mpsc::Sender, SinkExt, Stream};
use iced::{
	widget::{button, container, scrollable, text, Column},
	Element, Length, Subscription, Task, Theme,
};
use logger::Logger;
use modal::modal;
use rosc::{OscMessage, OscPacket, OscType};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use rust_decimal_macros::dec;
use std::{sync::Arc, time::Duration};
use tokio::net::UdpSocket;
use tracing::{debug, error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_unwrap::ResultExt;
use vrcc_core::Mask;

const MASK_COUNTER_PARAM: &str = "/avatar/parameters/mask_counter";
const MASK_ITERATION_PARAM: &str = "/avatar/parameters/mask_iteration";

// TODO: auto-run on steamvr
// TODO: add plotters-iced: https://github.com/joylei/plotters-iced
// TODO: add app to tray icon: https://github.com/tauri-apps/tray-icon
// TODO: add lilt: https://github.com/ejjonny/lilt
// TODO: add app icon
// TODO: auto-detect avatar parameters: $env:USERPROFILE\AppData\LocalLow\VRChat\VRChat\OSC\{user_id}\Avatars\{avatar_id}.json
fn main() -> iced::Result {
	iced::application("VRC Counter", Counter::update, Counter::view)
		.theme(Counter::theme)
		.subscription(Counter::subscription)
		.run_with(Counter::new)
}

/// A blend tree is used inside the Unity Editor and uses a float parameter with a minimum range of
/// negative one (-1) to a maximum range of positive one (+1). VRChat clamps remote parameters
/// across the network to two decimal places (0.99). This gives a possible accurate range of 200
/// values and this function is used to convert the integer form into the float that represents
/// that integer by returning a `Decimal`.
///
/// Note that the function is not aware of a minimum/maximum range, therefore a `Decimal` can be
/// returned with a value over positive one by giving a number greater than 200.
///
/// # Example
///
/// ```rust
/// let num = 200;
/// let dec = int_to_decimal(num).to_f32().unwrap();
///
/// assert_eq!(1.0, dec)
/// ```
fn int_to_decimal(num: usize) -> Decimal {
	let output = Decimal::new(num as i64, 0) * dec!(0.01);
	dec!(-1.0) + output
}

#[derive(Debug, Clone)]
enum ScreenKind {
	TestModal,
}

#[derive(Debug)]
enum Screen {
	TestModal(test_modal::TestModal),
}

#[derive(Debug, Clone)]
enum Event {
	CounterUpdated,
	Log(String),
}

#[derive(Debug)]
struct Counter {
	state: vrcc_core::State,
	mask_counter: usize,
	modal: Option<Screen>,
	logs: Vec<String>,
}

#[derive(Debug, Clone)]
enum Message {
	Event(Event),
	ModalChanged(ScreenKind),
	ModalClosed,
	TestModal(test_modal::Message),
}

impl Counter {
	fn new() -> (Self, Task<Message>) {
		let state = futures::executor::block_on(vrcc_core::State::new());

		let db = &state.db;
		let data =
			futures::executor::block_on(db.mask_counter().find_many(Vec::new()).exec()).unwrap();

		(
			Counter {
				state,
				mask_counter: data.len(),
				modal: None,
				logs: Vec::new(),
			},
			Task::none(),
		)
	}

	fn update(&mut self, message: Message) -> Task<Message> {
		match message {
			Message::Event(event) => match event {
				Event::CounterUpdated => {
					self.mask_counter += 1;
					Task::none()
				}
				Event::Log(value) => {
					self.logs.push(value);
					Task::none()
				}
			},
			Message::ModalChanged(kind) => match kind {
				ScreenKind::TestModal => {
					self.modal = Some(Screen::TestModal(test_modal::TestModal::new()));
					Task::none()
				}
			},
			Message::ModalClosed => {
				self.modal = None;
				Task::none()
			}
			Message::TestModal(message) => {
				let Some(screen) = &mut self.modal else {
					return Task::none();
				};
				match screen {
					Screen::TestModal(test) => {
						test.update(message);
						Task::none()
					}
					_ => Task::none(),
				}
			}
		}
	}

	fn view(&self) -> Element<Message> {
		let counter_text = text(self.mask_counter);
		let modal_button =
			button(text("Test Modal")).on_press(Message::ModalChanged(ScreenKind::TestModal));

		let content = container(Column::new().push(counter_text).push(modal_button));

		let logs = container(scrollable(Column::from_vec(
			self.logs.iter().map(|log| text(log).into()).collect(),
		)))
		.width(Length::Fill)
		.height(Length::Fill);

		let root_column = Column::new().push(content).push(logs);
		let root_container = container(root_column)
			.width(Length::Fill)
			.height(Length::Fill);

		if let Some(screen) = &self.modal {
			let Screen::TestModal(test) = screen;
			modal(root_container, test.view().map(Message::TestModal), || {
				Message::ModalClosed
			})
		} else {
			root_container.into()
		}
	}

	fn subscription(&self) -> iced::Subscription<Message> {
		let sub_logger = Subscription::run(log_stream).map(Message::Event);

		struct Listen;
		let sub_counter =
			Subscription::run_with_id(std::any::TypeId::of::<Listen>(), self.counter_stream())
				.map(Message::Event);

		Subscription::batch([sub_logger, sub_counter])
	}

	fn counter_stream(&self) -> impl Stream<Item = Event> {
		let db = Arc::clone(&self.state.db);
		let avatar_params = self.state.config.avatar_params.clone();

		// TODO: refactor redundant code
		// TODO: handle all unwraps to print to stdout ideally in a func that returns result
		iced::stream::channel(0, |mut tx: Sender<Event>| async move {
			// TODO: handle AddrInUse error
			let socket = UdpSocket::bind("127.0.0.1:9001").await.unwrap();

			// NOTE: get the start of the current day
			// let start_cur_date = Local::now()
			// 	.fixed_offset()
			// 	.with_hour(0)
			// 	.unwrap()
			// 	.with_minute(0)
			// 	.unwrap()
			// 	.with_second(0)
			// 	.unwrap()
			// 	.with_nanosecond(0)
			// 	.unwrap();

			let mut data_len = db
				.mask_counter()
				.find_many(vec![
					// NOTE: only select records within the current day and grabbed instead of posed
					// mask_counter::date::gt(start_cur_date),
					// mask_counter::WhereParam::Or(vec![
					// 	mask_counter::r#type::equals(
					// 		Mask::UpGrabbed(Regex::new("").unwrap()).discriminant() as i32,
					// 	),
					// 	mask_counter::r#type::equals(
					// 		Mask::DownGrabbed(Regex::new("").unwrap()).discriminant() as i32,
					// 	),
					// ]),
				])
				.exec()
				.await
				.unwrap()
				.len();
			let mut iteration_amount = 0;

			let mut buf = [0u8; rosc::decoder::MTU];
			loop {
				if data_len >= 200 {
					info!("Setting iteration_amount and data_len!");
					info!("iteration_amount: {}", iteration_amount);
					info!("data_len: {}", data_len);
					iteration_amount += data_len / 200;
					data_len %= 200;
					info!("iteration_amount: {}", iteration_amount);
					info!("data_len: {}", data_len);
					let output = int_to_decimal(iteration_amount);
					let iteration_buf = rosc::encoder::encode(&OscPacket::Message(OscMessage {
						addr: String::from(MASK_ITERATION_PARAM),
						args: vec![OscType::Float(output.to_f32().unwrap())],
					}))
					.unwrap();
					socket
						.send_to(&iteration_buf, "127.0.0.1:9000")
						.await
						.unwrap_or_log();
				}
				match socket.recv_from(&mut buf).await {
					Ok((size, addr)) => {
						debug!("Received packet with size {} from: {}", &size, &addr);
						let (_, packet) = rosc::decoder::decode_udp(&buf[..size]).unwrap();
						match packet {
							OscPacket::Message(msg) => {
								debug!("OSC address: {}", &msg.addr);
								debug!("OSC arguments: {:?}", &msg.args);
								if let Some(arg) = msg.args.first()
									&& let OscType::Bool(value) = arg
									&& *value
								{
									let addr = msg.addr.as_str();
									for param in &avatar_params {
										match param {
											Mask::UpPosed(regex) => {
												if regex.find(addr).is_some() {
													info!("posed up!");

													if let Err(e) = db
														.mask_counter()
														.create(
															param.discriminant() as i32,
															Vec::new(),
														)
														.exec()
														.await
													{
														error!("{}", e);
													} else {
														tx.send(Event::CounterUpdated)
															.await
															.unwrap();
													}
												}
											}
											Mask::DownPosed(regex) => {
												if regex.find(addr).is_some() {
													info!("posed down!");
													if let Err(e) = db
														.mask_counter()
														.create(
															param.discriminant() as i32,
															Vec::new(),
														)
														.exec()
														.await
													{
														error!("{}", e);
													} else {
														tx.send(Event::CounterUpdated)
															.await
															.unwrap();
													}
												}
											}
											Mask::UpGrabbed(regex) => {
												if regex.find(addr).is_some() {
													info!("grabbed up!");
													if let Err(e) = db
														.mask_counter()
														.create(
															param.discriminant() as i32,
															Vec::new(),
														)
														.exec()
														.await
													{
														error!("{}", e);
													} else {
														data_len += 1;

														let output = int_to_decimal(data_len);
														info!("output: {}", output);
														info!("from address: {}", &msg.addr);
														info!(
															"affected address: {}",
															MASK_COUNTER_PARAM
														);

														let counter_buf = rosc::encoder::encode(
															&OscPacket::Message(OscMessage {
																addr: String::from(
																	MASK_COUNTER_PARAM,
																),
																args: vec![OscType::Float(
																	output.to_f32().unwrap(),
																)],
															}),
														)
														.unwrap();
														if let Err(e) = socket
															.send_to(&counter_buf, "127.0.0.1:9000")
															.await
														{
															error!("{}", e);
														}

														tx.send(Event::CounterUpdated)
															.await
															.unwrap();
													}
												}
											}
											Mask::DownGrabbed(regex) => {
												if regex.find(addr).is_some() {
													info!("grabbed down!");
													if let Err(e) = db
														.mask_counter()
														.create(
															param.discriminant() as i32,
															Vec::new(),
														)
														.exec()
														.await
													{
														error!("{}", e);
													} else {
														data_len += 1;

														let output = int_to_decimal(data_len);
														info!("output: {}", output);
														info!("from address: {}", &msg.addr);
														info!(
															"affected address: {}",
															MASK_COUNTER_PARAM
														);

														let counter_buf = rosc::encoder::encode(
															&OscPacket::Message(OscMessage {
																addr: String::from(
																	MASK_COUNTER_PARAM,
																),
																args: vec![OscType::Float(
																	output.to_f32().unwrap(),
																)],
															}),
														)
														.unwrap();
														if let Err(e) = socket
															.send_to(&counter_buf, "127.0.0.1:9000")
															.await
														{
															error!("{}", e);
														}

														tx.send(Event::CounterUpdated)
															.await
															.unwrap();
													}
												}
											}
										}
									}
								} else if msg.addr == "/avatar/change" {
									// TODO: configure avatar ids

									let output = int_to_decimal(data_len);
									info!("output: {}", output);
									info!("from address: {}", &msg.addr);
									info!("affected address: {}", MASK_COUNTER_PARAM);

									let counter_buf =
										rosc::encoder::encode(&OscPacket::Message(OscMessage {
											addr: String::from(MASK_COUNTER_PARAM),
											args: vec![OscType::Float(output.to_f32().unwrap())],
										}))
										.unwrap();
									if let Err(e) =
										socket.send_to(&counter_buf, "127.0.0.1:9000").await
									{
										error!("{}", e);
									}
									info!("iteration_amount: {}", iteration_amount);
									let output = int_to_decimal(iteration_amount);
									let iteration_buf =
										rosc::encoder::encode(&OscPacket::Message(OscMessage {
											addr: String::from(MASK_ITERATION_PARAM),
											args: vec![OscType::Float(output.to_f32().unwrap())],
										}))
										.unwrap();
									if let Err(e) =
										socket.send_to(&iteration_buf, "127.0.0.1:9000").await
									{
										error!("{}", e);
									}
								}
							}
							OscPacket::Bundle(bundle) => {
								debug!("OSC Bundle: {:?}", &bundle);
							}
						}
					}
					Err(e) => {
						error!("Error receiving from socket: {}", e);
					}
				}
			}
		})
	}

	fn theme(&self) -> Theme {
		Theme::CatppuccinFrappe
	}
}

fn log_stream() -> impl Stream<Item = Event> {
	iced::stream::channel(0, |tx: Sender<Event>| async move {
		tracing_subscriber::registry()
			.with(Logger::new(tx).with_max_level(tracing::Level::INFO))
			.init();

		loop {
			tokio::time::sleep(Duration::new(1, 0)).await;
		}
	})
}

// TODO: move modules into their own files
mod test_modal {
	use iced::{
		widget::{container, text, Column},
		Element,
	};

	#[derive(Debug)]
	pub struct TestModal;

	#[derive(Debug, Clone)]
	pub enum Message {}

	impl TestModal {
		pub fn new() -> Self {
			Self {}
		}

		pub fn update(&mut self, _message: Message) {}

		pub fn view(&self) -> Element<Message> {
			let text_color = iced::theme::palette::Palette::CATPPUCCIN_FRAPPE.text;
			let bg_color = iced::theme::palette::Palette::CATPPUCCIN_FRAPPE.background;

			container(Column::new().push(text("Hello modal!")).spacing(20))
				.width(300)
				.padding(10)
				.style(move |_theme| container::Style {
					text_color: Some(text_color),
					background: Some(iced::Background::Color(bg_color)),
					border: iced::Border {
						radius: 8.0.into(),
						..Default::default()
					},
					..Default::default()
				})
				.into()
		}
	}
}

// TODO: add animations with lilt
mod modal {
	//! License SPDX: GPL-3.0-only
	//! Source: https://github.com/squidowl/halloy/blob/main/src/widget/modal.rs

	use iced::advanced::layout::{self, Layout};
	use iced::advanced::overlay;
	use iced::advanced::renderer;
	use iced::advanced::widget::{self, Widget};
	use iced::advanced::{self, Clipboard, Shell};
	use iced::alignment::Alignment;
	use iced::keyboard::key;
	use iced::mouse;
	use iced::{event, keyboard};
	use iced::{Color, Element, Event, Length, Point, Rectangle, Size, Vector};

	pub fn modal<'a, Message, Theme, Renderer>(
		base: impl Into<Element<'a, Message, Theme, Renderer>>,
		modal: impl Into<Element<'a, Message, Theme, Renderer>>,
		on_blur: impl Fn() -> Message + 'a,
	) -> Element<'a, Message, Theme, Renderer>
	where
		Theme: 'a,
		Renderer: 'a + advanced::Renderer,
		Message: 'a,
	{
		Modal::new(base, modal, on_blur).into()
	}

	/// A widget that centers a modal element over some base element
	pub struct Modal<'a, Message, Theme, Renderer> {
		base: Element<'a, Message, Theme, Renderer>,
		modal: Element<'a, Message, Theme, Renderer>,
		on_blur: Box<dyn Fn() -> Message + 'a>,
	}

	impl<'a, Message, Theme, Renderer> Modal<'a, Message, Theme, Renderer> {
		/// Returns a new [`Modal`]
		pub fn new(
			base: impl Into<Element<'a, Message, Theme, Renderer>>,
			modal: impl Into<Element<'a, Message, Theme, Renderer>>,
			on_blur: impl Fn() -> Message + 'a,
		) -> Self {
			Self {
				base: base.into(),
				modal: modal.into(),
				on_blur: Box::new(on_blur),
			}
		}
	}

	impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
		for Modal<'a, Message, Theme, Renderer>
	where
		Renderer: advanced::Renderer,
	{
		fn children(&self) -> Vec<widget::Tree> {
			vec![
				widget::Tree::new(&self.base),
				widget::Tree::new(&self.modal),
			]
		}

		fn diff(&self, tree: &mut widget::Tree) {
			tree.diff_children(&[&self.base, &self.modal]);
		}

		fn size(&self) -> Size<Length> {
			self.base.as_widget().size()
		}

		fn layout(
			&self,
			tree: &mut widget::Tree,
			renderer: &Renderer,
			limits: &layout::Limits,
		) -> layout::Node {
			self.base
				.as_widget()
				.layout(&mut tree.children[0], renderer, limits)
		}

		fn on_event(
			&mut self,
			state: &mut widget::Tree,
			event: Event,
			layout: Layout<'_>,
			cursor: mouse::Cursor,
			renderer: &Renderer,
			clipboard: &mut dyn Clipboard,
			shell: &mut Shell<'_, Message>,
			viewport: &Rectangle,
		) -> event::Status {
			self.base.as_widget_mut().on_event(
				&mut state.children[0],
				event,
				layout,
				cursor,
				renderer,
				clipboard,
				shell,
				viewport,
			)
		}

		fn draw(
			&self,
			state: &widget::Tree,
			renderer: &mut Renderer,
			theme: &Theme,
			style: &renderer::Style,
			layout: Layout<'_>,
			cursor: mouse::Cursor,
			viewport: &Rectangle,
		) {
			self.base.as_widget().draw(
				&state.children[0],
				renderer,
				theme,
				style,
				layout,
				cursor,
				viewport,
			);
		}

		fn overlay<'b>(
			&'b mut self,
			state: &'b mut widget::Tree,
			layout: Layout<'_>,
			_renderer: &Renderer,
			translation: Vector,
		) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
			Some(overlay::Element::new(Box::new(Overlay {
				position: layout.position() + translation,
				content: &mut self.modal,
				tree: &mut state.children[1],
				size: layout.bounds().size(),
				on_blur: &self.on_blur,
			})))
		}

		fn mouse_interaction(
			&self,
			state: &widget::Tree,
			layout: Layout<'_>,
			cursor: mouse::Cursor,
			viewport: &Rectangle,
			renderer: &Renderer,
		) -> mouse::Interaction {
			self.base.as_widget().mouse_interaction(
				&state.children[0],
				layout,
				cursor,
				viewport,
				renderer,
			)
		}

		fn operate(
			&self,
			state: &mut widget::Tree,
			layout: Layout<'_>,
			renderer: &Renderer,
			operation: &mut dyn widget::Operation<()>,
		) {
			self.base
				.as_widget()
				.operate(&mut state.children[0], layout, renderer, operation);
		}
	}

	struct Overlay<'a, 'b, Message, Theme, Renderer> {
		position: Point,
		content: &'b mut Element<'a, Message, Theme, Renderer>,
		tree: &'b mut widget::Tree,
		size: Size,
		on_blur: &'b dyn Fn() -> Message,
	}

	impl<'a, 'b, Message, Theme, Renderer> overlay::Overlay<Message, Theme, Renderer>
		for Overlay<'a, 'b, Message, Theme, Renderer>
	where
		Renderer: advanced::Renderer,
	{
		fn layout(&mut self, renderer: &Renderer, _bounds: Size) -> layout::Node {
			let limits = layout::Limits::new(Size::ZERO, self.size)
				.width(Length::Fill)
				.height(Length::Fill);

			let child = self
				.content
				.as_widget()
				.layout(self.tree, renderer, &limits)
				.align(Alignment::Center, Alignment::Center, limits.max());

			layout::Node::with_children(self.size, vec![child]).move_to(self.position)
		}

		fn on_event(
			&mut self,
			event: Event,
			layout: Layout<'_>,
			cursor: mouse::Cursor,
			renderer: &Renderer,
			clipboard: &mut dyn Clipboard,
			shell: &mut Shell<'_, Message>,
		) -> event::Status {
			match event {
				Event::Keyboard(keyboard::Event::KeyPressed {
					key: keyboard::Key::Named(key::Named::Escape),
					..
				}) => {
					shell.publish((self.on_blur)());
					return event::Status::Captured;
				}
				Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
					let bounds = layout.children().next().unwrap().bounds();

					if !cursor.is_over(bounds) {
						shell.publish((self.on_blur)());
						return event::Status::Captured;
					}
				}
				_ => {}
			}

			self.content.as_widget_mut().on_event(
				self.tree,
				event,
				layout.children().next().unwrap(),
				cursor,
				renderer,
				clipboard,
				shell,
				&layout.bounds(),
			)
		}

		fn draw(
			&self,
			renderer: &mut Renderer,
			theme: &Theme,
			style: &renderer::Style,
			layout: Layout<'_>,
			cursor: mouse::Cursor,
		) {
			renderer.fill_quad(
				renderer::Quad {
					bounds: layout.bounds(),
					..renderer::Quad::default()
				},
				Color {
					a: 0.80,
					..Color::BLACK
				},
			);

			self.content.as_widget().draw(
				self.tree,
				renderer,
				theme,
				style,
				layout.children().next().unwrap(),
				cursor,
				&layout.bounds(),
			);
		}

		fn operate(
			&mut self,
			layout: Layout<'_>,
			renderer: &Renderer,
			operation: &mut dyn widget::Operation<()>,
		) {
			self.content.as_widget().operate(
				self.tree,
				layout.children().next().unwrap(),
				renderer,
				operation,
			);
		}

		fn mouse_interaction(
			&self,
			layout: Layout<'_>,
			cursor: mouse::Cursor,
			viewport: &Rectangle,
			renderer: &Renderer,
		) -> mouse::Interaction {
			self.content.as_widget().mouse_interaction(
				self.tree,
				layout.children().next().unwrap(),
				cursor,
				viewport,
				renderer,
			)
		}

		fn overlay<'c>(
			&'c mut self,
			layout: Layout<'_>,
			renderer: &Renderer,
		) -> Option<overlay::Element<'c, Message, Theme, Renderer>> {
			self.content.as_widget_mut().overlay(
				self.tree,
				layout.children().next().unwrap(),
				renderer,
				Vector::ZERO,
			)
		}
	}

	impl<'a, Message, Theme, Renderer> From<Modal<'a, Message, Theme, Renderer>>
		for Element<'a, Message, Theme, Renderer>
	where
		Theme: 'a,
		Renderer: 'a + advanced::Renderer,
		Message: 'a,
	{
		fn from(modal: Modal<'a, Message, Theme, Renderer>) -> Self {
			Element::new(modal)
		}
	}
}
