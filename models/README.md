# Models

MediaPipe Selfie Segmentation, used for the webcam's virtual background
(`src/background.rs`), originally by Google
([MediaPipe](https://github.com/google-ai-edge/mediapipe)). License: Apache-2.0.
ONNX conversions from onnx-community (`onnx/model.onnx` in each):

| File | Model | Input | sha256 |
|------|-------|-------|--------|
| `selfie_segmentation_landscape.onnx` | [landscape](https://huggingface.co/onnx-community/mediapipe_selfie_segmentation_landscape), for landscape pictures | 256x144 | `7a0adcfdb1715d3b0ff0f61486d8a181c33f4a208343a5abfccbc6540e872d24` |
| `selfie_segmentation.onnx` | [general](https://huggingface.co/onnx-community/mediapipe_selfie_segmentation), for portrait pictures | 256x256 | `3241ac4ad8aa35bdaf33946776db29f7c283a413aa0b0dacb9483594b4531aad` |

Input `[1, 3, H, W]` RGB in 0..1 (NCHW), the picture stretched to the model
size; output `[1, 1, H, W]`, the probability that each pixel is a person.
