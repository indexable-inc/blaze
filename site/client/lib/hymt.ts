/** The chat translator's model, at a pinned revision (client/translate-worker.ts loads it, client/lib/translate.ts checks the cache for it). */
export const MODEL = 'onnx-community/HY-MT1.5-1.8B-ONNX';
export const REVISION = '2f11819b25de08cecd344735cdfa5136ade41a67';
/** Where transformers.js keeps a downloaded file of it, in Cache Storage `transformers-cache`. */
export const modelFile = (file: string) => `https://huggingface.co/${MODEL}/resolve/${REVISION}/${file}`;
