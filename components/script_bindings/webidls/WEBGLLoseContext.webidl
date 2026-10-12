/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/WEBGL_lose_context/
 */

// skip-unless CARGO_FEATURE_WEBGL

[LegacyNoInterfaceObject, Exposed=Window]
interface WEBGLLoseContext {
  undefined loseContext();
  undefined restoreContext();
}; // interface WEBGL_lose_context
