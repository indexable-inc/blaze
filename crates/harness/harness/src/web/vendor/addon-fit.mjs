/**
 * Copyright (c) 2014-2024 The xterm.js authors. All rights reserved.
 * @license MIT
 *
 * Copyright (c) 2012-2013, Christopher Jeffrey (MIT License)
 * @license MIT
 *
 * Originally forked from (with the author's permission):
 *   Fabrice Bellard's javascript vt100 for jslinux:
 *   http://bellard.org/jslinux/
 *   Copyright (c) 2011 Fabrice Bellard
 */
/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/
function f(t){return t?.ownerDocument?.defaultView?t.ownerDocument.defaultView:window}function o(t){return f(t).getComputedStyle(t,null)}var l=class{activate(e){this._terminal=e}dispose(){}fit(){let e=this.proposeDimensions();!e||!this._terminal||isNaN(e.cols)||isNaN(e.rows)||this._terminal.resize(e.cols,e.rows)}proposeDimensions(){if(!this._terminal||!this._terminal.element||!this._terminal.element.parentElement)return;let e=this._terminal.dimensions;if(!e||e.css.cell.width===0||e.css.cell.height===0)return;let s=this._terminal.options.scrollbar?.showScrollbar??!0,a=this._terminal.options.scrollback===0||!s?0:this._terminal.options.scrollbar?.width??14,r=o(this._terminal.element.parentElement),m=Math.max(0,parseInt(r.getPropertyValue("height"),10)||0),d=Math.max(0,parseInt(r.getPropertyValue("width"),10)||0),n=o(this._terminal.element),i={top:parseInt(n.getPropertyValue("padding-top"),10)||0,bottom:parseInt(n.getPropertyValue("padding-bottom"),10)||0,right:parseInt(n.getPropertyValue("padding-right"),10)||0,left:parseInt(n.getPropertyValue("padding-left"),10)||0},p=i.top+i.bottom,c=i.right+i.left,h=m-p,u=d-c-a;return{cols:Math.max(2,Math.floor(u/e.css.cell.width)),rows:Math.max(1,Math.floor(h/e.css.cell.height))}}};export{l as FitAddon};
//# sourceMappingURL=addon-fit.mjs.map
