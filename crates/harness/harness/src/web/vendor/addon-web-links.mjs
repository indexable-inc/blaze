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
var u=class{constructor(t,e,n,o={}){this._terminal=t;this._regex=e;this._handler=n;this._options=o}provideLinks(t,e){let n=f.computeLink(t,this._regex,this._terminal,this._handler);e(this._addCallbacks(n))}_addCallbacks(t){return t.map(e=>(e.leave=this._options.leave,e.hover=(n,o)=>{if(this._options.hover){let{range:p}=e;this._options.hover(n,o,p)}},e))}};function _(l){try{let t=new URL(l),e=t.password&&t.username?`${t.protocol}//${t.username}:${t.password}@${t.host}`:t.username?`${t.protocol}//${t.username}@${t.host}`:`${t.protocol}//${t.host}`;return l.toLocaleLowerCase().startsWith(e.toLocaleLowerCase())}catch{return!1}}var f=class l{static computeLink(t,e,n,o){let p=e.flags.includes("g")?e.flags:`${e.flags}g`,i=new RegExp(e.source,p),[r,s]=l._getWindowedLineStrings(t-1,n),a=r.join(""),d,v=[];for(;d=i.exec(a);){let c=d[0];if(!_(c))continue;let[h,g]=l._mapStrIdx(n,s,0,d.index),[m,L]=l._mapStrIdx(n,h,g,c.length);if(h===-1||g===-1||m===-1||L===-1)continue;let k={start:{x:g+1,y:h+1},end:{x:L,y:m+1}};v.push({range:k,text:c,activate:o})}return v}static _getWindowedLineStrings(t,e){let n,o=t,p=t,i,r,s=[];if(n=e.buffer.active.getLine(t)){let a=n.translateToString(!0);if(n.isWrapped&&a[0]!==" "){for(i=0;(n=e.buffer.active.getLine(--o))&&i<2048&&(r=n.translateToString(!0),i+=r.length,s.push(r),!(!n.isWrapped||r.indexOf(" ")!==-1)););s.reverse()}for(s.push(a),i=0;(n=e.buffer.active.getLine(++p))&&n.isWrapped&&i<2048&&(r=n.translateToString(!0),i+=r.length,s.push(r),r.indexOf(" ")===-1););}return[s,o]}static _mapStrIdx(t,e,n,o){let p=t.buffer.active,i=p.getNullCell(),r=n;for(;o;){let s=p.getLine(e);if(!s)return[-1,-1];for(let a=r;a<s.length;++a){s.getCell(a,i);let d=i.getChars();if(i.getWidth()&&(o-=d.length||1,a===s.length-1&&d==="")){let c=p.getLine(e+1);c&&c.isWrapped&&(c.getCell(0,i),i.getWidth()===2&&(o+=1))}if(o<0)return[e,a]}e++,r=0}return[e,r]}};var w=/(https?|HTTPS?):[/]{2}[^\s"'!*(){}|\\\^<>`]*[^\s"':,.!?{}|\\\^~\[\]`()<>]/;function x(l,t){let e=window.open();if(e){try{e.opener=null}catch{}e.location.href=t}else console.warn("Opening link blocked as opener could not be cleared")}var b=class{constructor(t=x,e={}){this._handler=t;this._options=e}activate(t){this._terminal=t;let e=this._options,n=e.urlRegex??w;this._linkProvider=this._terminal.registerLinkProvider(new u(this._terminal,n,this._handler,e))}dispose(){this._linkProvider?.dispose()}};export{b as WebLinksAddon};
//# sourceMappingURL=addon-web-links.mjs.map
