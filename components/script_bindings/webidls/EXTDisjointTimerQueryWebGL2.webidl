/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * WebGL IDL definitions from the Khronos specification:
 * https://registry.khronos.org/webgl/extensions/EXT_disjoint_timer_query_webgl2/
 */

// skip-unless CARGO_FEATURE_WEBGL

[LegacyNoInterfaceObject, Exposed=Window]
interface EXTDisjointTimerQueryWebGL2 {
  const GLenum QUERY_COUNTER_BITS_EXT = 0x8864;
  const GLenum TIME_ELAPSED_EXT = 0x88BF;
  const GLenum TIMESTAMP_EXT = 0x8E28;
  const GLenum GPU_DISJOINT_EXT = 0x8FBB;

  undefined queryCounterEXT(WebGLQuery query, GLenum target);
}; // interface EXT_disjoint_timer_query_webgl2
