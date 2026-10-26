# Models

`selfie_segmentation_landscape.onnx`: MediaPipe Selfie Segmentation
(landscape, 256x144), used for the webcam's virtual background
(`src/background.rs`). ONNX conversion from
[onnx-community/mediapipe_selfie_segmentation_landscape](https://huggingface.co/onnx-community/mediapipe_selfie_segmentation_landscape)
(`onnx/model.onnx`), originally by Google
([MediaPipe](https://github.com/google-ai-edge/mediapipe)).

License: Apache-2.0. sha256
`7a0adcfdb1715d3b0ff0f61486d8a181c33f4a208343a5abfccbc6540e872d24`.

Input `[1, 3, 144, 256]` RGB in 0..1 (NCHW); output `[1, 1, 144, 256]`, the
probability that each pixel is a person.
