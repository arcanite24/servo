/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::WebGLVersion;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::codegen::Bindings::EXTDisjointTimerQueryWebGL2Binding::EXTDisjointTimerQueryWebGL2Methods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglquery::WebGLQuery;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/EXT_disjoint_timer_query_webgl2/>: GPU time through
/// `TIME_ELAPSED_EXT` queries and `queryCounterEXT` timestamps. The WebGL 2 context handles the query targets once
/// this is enabled. Desktop OpenGL reports no disjoint events, so `GPU_DISJOINT_EXT` reads false there.
#[dom_struct]
pub(crate) struct EXTDisjointTimerQueryWebGL2 {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl EXTDisjointTimerQueryWebGL2 {
    fn new_inherited(ctx: &WebGLRenderingContext) -> Self {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }
}

impl WebGLExtension for EXTDisjointTimerQueryWebGL2 {
    type Extension = EXTDisjointTimerQueryWebGL2;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited(ctx)), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        // Timer queries are core in desktop OpenGL 3.3, which every WebGL 2 context runs on; OpenGL ES (ANGLE
        // included) needs the extension.
        !ext.is_gles() || ext.supports_any_gl_extension(&["GL_EXT_disjoint_timer_query"])
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "EXT_disjoint_timer_query_webgl2"
    }
}

impl EXTDisjointTimerQueryWebGL2Methods<crate::DomTypeHolder> for EXTDisjointTimerQueryWebGL2 {
    /// <https://registry.khronos.org/webgl/extensions/EXT_disjoint_timer_query_webgl2/>
    fn QueryCounterEXT(&self, query: &WebGLQuery, target: u32) {
        handle_potential_webgl_error!(self.ctx, self.ctx.validate_ownership(query), return);
        if let Err(error) = query.counter(&self.ctx, target) {
            self.ctx.webgl_error(error);
        }
    }
}
