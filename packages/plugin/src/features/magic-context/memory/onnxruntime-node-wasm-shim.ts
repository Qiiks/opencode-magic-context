// The injected Symbol.for("onnxruntime") runtime is authoritative. This shim
// keeps Transformers.js's static Node import loadable on platforms where the
// optional native addon is absent, without evaluating onnxruntime-node.
import * as ortWeb from "onnxruntime-web";

export default ortWeb;
