const status = document.getElementById("status");
const video = document.getElementById("video");
const startButton = document.getElementById("start-session");

const pc = new RTCPeerConnection();
const remoteStream = new MediaStream();
video.srcObject = remoteStream;

pc.addTransceiver("video", { direction: "recvonly" });
pc.addTransceiver("audio", { direction: "recvonly" });

pc.ontrack = (event) => {
    remoteStream.addTrack(event.track);
};

pc.onconnectionstatechange = () => {
    status.textContent = pc.connectionState;
};

pc.oniceconnectionstatechange = () => {
    status.textContent = `${pc.connectionState} / ${pc.iceConnectionState}`;
};

async function waitForIceGatheringComplete(timeoutMs = 1000) {
    if (pc.iceGatheringState === "complete") {
        return;
    }

    await Promise.race([
        new Promise((resolve) => {
            pc.addEventListener("icegatheringstatechange", () => {
                if (pc.iceGatheringState === "complete") {
                    resolve();
                }
            });
        }),
        new Promise((resolve) => setTimeout(resolve, timeoutMs)),
    ]);
}

startButton.onclick = async () => {
    try {
        startButton.disabled = true;
        status.textContent = "Creating offer";

        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        await waitForIceGatheringComplete();

        status.textContent = "Sending offer";
        const response = await fetch("/offer", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(pc.localDescription),
        });

        if (!response.ok) {
            throw new Error(await response.text());
        }

        status.textContent = "Applying answer";
        await pc.setRemoteDescription(await response.json());
        await video.play();
    } catch (error) {
        status.textContent = error.message;
        startButton.disabled = false;
    }
};
