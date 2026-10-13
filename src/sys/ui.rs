//! The menu bar item and its menu. AppKit, so the main thread only.

use std::sync::{Arc, atomic::Ordering, mpsc::Sender};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, Sel},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn,
    NSImage, NSMenu, NSMenuItem, NSSlider, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, ns_string};

use crate::{
    ir::{HALL, SEATS, STAGES},
    state::{Event, Params, Selection},
};

struct Ivars {
    events: Sender<Event>,
    params: Arc<Params>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and the class does
    // not implement `Drop`
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "Boom3DMenuTarget"]
    #[ivars = Ivars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(toggleEnabled:))]
        fn toggle_enabled(&self, item: &NSMenuItem) {
            let enabled = item.state() != NSControlStateValueOn;
            item.setState(state(enabled));
            self.send(Event::Enabled(enabled));
        }

        #[unsafe(method(wetChanged:))]
        fn wet_changed(&self, slider: &NSSlider) {
            self.ivars().params.wet.set(slider.floatValue());
        }

        #[unsafe(method(toggleTracking:))]
        fn toggle_tracking(&self, item: &NSMenuItem) {
            let tracking = item.state() != NSControlStateValueOn;
            item.setState(state(tracking));
            self.ivars().params.tracking.store(tracking, Ordering::Relaxed);
        }

        #[unsafe(method(faceStage:))]
        fn face_stage(&self, _item: &NSMenuItem) {
            self.ivars().params.face_stage.store(true, Ordering::Relaxed);
        }

        #[unsafe(method(selectStage:))]
        fn select_stage(&self, item: &NSMenuItem) {
            check_only(item);
            self.send(Event::Stage(item.tag() as usize));
        }

        #[unsafe(method(selectSeat:))]
        fn select_seat(&self, item: &NSMenuItem) {
            check_only(item);
            self.send(Event::Seat(item.tag() as usize));
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _item: &NSMenuItem) {
            // The control thread stops the audio and ends the process
            self.send(Event::Shutdown);
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: Event) {
        let _ = self.ivars().events.send(event);
    }
}

fn state(on: bool) -> isize {
    if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

/// Checks `item` and unchecks the other items of its menu.
fn check_only(item: &NSMenuItem) {
    // The menu an item is in outlives the item's action
    if let Some(menu) = unsafe { item.menu() } {
        for other in menu.itemArray() {
            other.setState(NSControlStateValueOff);
        }
    }
    item.setState(NSControlStateValueOn);
}

/// What the items of a menu are built with.
struct Builder<'a> {
    mtm: MainThreadMarker,
    target: &'a MenuTarget,
}

impl Builder<'_> {
    fn menu(&self) -> Retained<NSMenu> {
        NSMenu::initWithTitle(self.mtm.alloc(), ns_string!(""))
    }

    fn item(&self, title: &str, action: Sel) -> Retained<NSMenuItem> {
        let target: &AnyObject = self.target.as_ref();
        unsafe {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                self.mtm.alloc(),
                &NSString::from_str(title),
                Some(action),
                ns_string!(""),
            );
            item.setTarget(Some(target));
            item
        }
    }

    fn header(&self, title: &str) -> Retained<NSMenuItem> {
        NSMenuItem::sectionHeaderWithTitle(&NSString::from_str(title), self.mtm)
    }

    /// An item that opens a list to pick one of `names` from.
    fn choice(
        &self,
        title: &str,
        names: impl Iterator<Item = &'static str>,
        selected: usize,
        action: Sel,
    ) -> Retained<NSMenuItem> {
        let menu = self.menu();
        for (index, name) in names.enumerate() {
            let item = self.item(name, action);
            item.setTag(index as isize);
            item.setState(state(index == selected));
            menu.addItem(&item);
        }
        let item = NSMenuItem::new(self.mtm);
        item.setTitle(&NSString::from_str(title));
        item.setSubmenu(Some(&menu));
        item
    }

    /// An item holding a slider from 0 to 1.
    fn slider(&self, value: f32, action: Sel) -> Retained<NSMenuItem> {
        let target: &AnyObject = self.target.as_ref();
        let frame =
            |x, y, width, height| NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));

        let slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                f64::from(value),
                0.0,
                1.0,
                Some(target),
                Some(action),
                self.mtm,
            )
        };
        slider.setContinuous(true);
        slider.setFrame(frame(16.0, 4.0, 188.0, 24.0));

        let view = NSView::initWithFrame(self.mtm.alloc(), frame(0.0, 0.0, 220.0, 32.0));
        view.addSubview(&slider);
        let item = NSMenuItem::new(self.mtm);
        item.setView(Some(&view));
        item
    }
}

fn status_item(mtm: MainThreadMarker, menu: &NSMenu) -> Retained<NSStatusItem> {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        let symbol = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            ns_string!("building.columns"),
            Some(ns_string!("Boom3D")),
        );
        match symbol {
            Some(symbol) => {
                symbol.setTemplate(true);
                button.setImage(Some(&symbol));
            }
            None => button.setTitle(ns_string!("Boom3D")),
        }
    }
    item.setMenu(Some(menu));
    item
}

/// Shows the menu bar item and runs the app's event loop. Never returns: the
/// control thread ends the process.
///
/// `selection` is what the menu starts out showing. Choices made in the menu
/// go to `events`, except the wet level and head tracking, which go straight
/// into `params`.
pub fn run(selection: Selection, events: Sender<Event>, params: Arc<Params>) -> ! {
    let mtm = MainThreadMarker::new().expect("the menu runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // In the menu bar only: no Dock icon, no main menu
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let wet = params.wet.get();
    let tracking = params.tracking.load(Ordering::Relaxed);
    let target = MenuTarget::new(mtm, Ivars { events, params });
    let build = Builder {
        mtm,
        target: &target,
    };

    let menu = build.menu();
    let enabled = build.item("켜기", sel!(toggleEnabled:));
    enabled.setState(state(selection.enabled));
    menu.addItem(&enabled);

    menu.addItem(&build.header("울림"));
    menu.addItem(&build.slider(wet, sel!(wetChanged:)));

    menu.addItem(&build.header(HALL));
    let stages = STAGES.iter().map(|stage| stage.name);
    menu.addItem(&build.choice("무대 폭", stages, selection.stage, sel!(selectStage:)));
    let seats = SEATS.iter().map(|seat| seat.name);
    menu.addItem(&build.choice("좌석", seats, selection.seat, sel!(selectSeat:)));

    menu.addItem(&build.header("머리 방향"));
    let follow = build.item("헤드트래킹", sel!(toggleTracking:));
    follow.setState(state(tracking));
    menu.addItem(&follow);
    menu.addItem(&build.item("지금 보는 쪽을 정면으로", sel!(faceStage:)));

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&build.item("종료", sel!(quit:)));

    // The status bar doesn't keep its items, and items don't keep their target
    let _status_item = status_item(mtm, &menu);
    app.run();
    unreachable!("the control thread ends the process");
}
