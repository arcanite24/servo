/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Pointer Lock API: <https://w3c.github.io/pointerlock/>.
//!
//! The document holds the lock and the embedder holds the cursor. Locking sends
//! [`EmbedderMsg::SetPointerLock`]; the embedder then hides and holds the cursor and reports raw
//! motion as `MouseMoveEvent::movement`, which reaches script as `movementX/Y` on `mousemove`
//! events targeted at the lock element. Escape releases the lock, as in other engines.

use std::rc::Rc;

use embedder_traits::EmbedderMsg;
use js::realm::CurrentRealm;
use stylo_atoms::Atom;

use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::document::Document;
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::node::node::Node;
use crate::dom::promise::Promise;

impl Document {
    /// <https://w3c.github.io/pointerlock/#dom-element-requestpointerlock>
    pub(crate) fn request_pointer_lock(
        &self,
        cx: &mut CurrentRealm,
        element: &Element,
    ) -> Rc<Promise> {
        let promise = Promise::new_in_realm(cx);

        // Refuse a lock for an element outside a fully active document, as other engines do.
        // There is no engagement gesture requirement: the embedder decides whether to grant
        // the cursor.
        if !self.is_fully_active() || !element.upcast::<Node>().is_connected() {
            self.queue_pointer_lock_event(Atom::from("pointerlockerror"));
            promise.reject_error(
                cx,
                Error::WrongDocument(Some(
                    "Pointer lock target is not connected to a fully active document".to_owned(),
                )),
            );
            return promise;
        }

        let changed = self
            .pointer_lock_element()
            .is_none_or(|current| &*current != element);
        self.set_pointer_lock_element(Some(element));
        if changed {
            self.send_to_embedder(EmbedderMsg::SetPointerLock(self.webview_id(), true));
            self.queue_pointer_lock_event(Atom::from("pointerlockchange"));
        }
        promise.resolve_native(cx, &());
        promise
    }

    /// <https://w3c.github.io/pointerlock/#dom-document-exitpointerlock>
    pub(crate) fn exit_pointer_lock(&self) {
        if self.pointer_lock_element().is_none() {
            return;
        }
        self.set_pointer_lock_element(None);
        self.send_to_embedder(EmbedderMsg::SetPointerLock(self.webview_id(), false));
        self.queue_pointer_lock_event(Atom::from("pointerlockchange"));
    }

    /// The lock element, released first if it has left the document.
    pub(crate) fn connected_pointer_lock_element(&self) -> Option<DomRoot<Element>> {
        let element = self.pointer_lock_element()?;
        if element.upcast::<Node>().is_connected() {
            return Some(element);
        }
        self.exit_pointer_lock();
        None
    }

    fn queue_pointer_lock_event(&self, name: Atom) {
        self.owner_global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue_simple_event(self.upcast(), name);
    }
}
