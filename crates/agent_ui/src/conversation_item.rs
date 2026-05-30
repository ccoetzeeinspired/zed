//! FORK: agent-in-center (Stage 1 — see plans/agent-in-center.md).
//!
//! Hosts the agent conversation as a center-pane [`Item`] (tab), so the agent
//! can live in the literal center alongside editors and the browser tab — not
//! only in a side dock.
//!
//! [`ConversationItem`] wraps the SAME `Entity<ConversationView>` that
//! `AgentPanel` owns (via `active_conversation_view()` / `retained_threads`),
//! so the dock view and this center tab render the same conversation and stay
//! in sync through GPUI's entity-change broadcast. `AgentPanel` remains the
//! owner; closing this tab does not touch the thread (`discarded` is a no-op).

use crate::ConversationView;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, WeakEntity, Window, div,
};
use ui::{Icon, IconName};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

/// A center-pane tab that displays an agent [`ConversationView`].
pub struct ConversationItem {
    conversation: Entity<ConversationView>,
    // Captured for future stages (e.g. drag-between-regions); unused in Stage 1.
    #[allow(dead_code)]
    workspace: WeakEntity<Workspace>,
    /// Refreshes the tab (title) when the conversation changes.
    _observe_conversation: Subscription,
}

impl ConversationItem {
    pub fn new(
        conversation: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observe_conversation = cx.observe(&conversation, |_this, _conversation, cx| {
            cx.emit(ConversationItemEvent::UpdateTab);
        });
        Self {
            conversation,
            workspace,
            _observe_conversation,
        }
    }
}

pub enum ConversationItemEvent {
    UpdateTab,
}

impl EventEmitter<ConversationItemEvent> for ConversationItem {}

impl Focusable for ConversationItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // Delegate focus to the conversation so its message input receives it.
        self.conversation.read(cx).focus_handle(cx)
    }
}

impl Render for ConversationItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.conversation.clone())
    }
}

impl Item for ConversationItem {
    type Event = ConversationItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ConversationItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation.read(cx).title(cx)
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAssistant))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.conversation.read(cx).title(cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.workspace = workspace.weak_handle();
    }
}
