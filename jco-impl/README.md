# jco-impl

Browser-first (Node) host implementation of `polymorph:webrtc-datachannels`, using
jco to transpile the guest component and `node-datachannel` (or the browser's native
`RTCPeerConnection`) for the WebRTC data channel.
