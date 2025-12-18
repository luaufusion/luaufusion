/**
 * Represents the bridge interface itself
 */
interface Bridge {
    /**
     * Send a text message over the bridge
     * 
     * sendText uses the text channel to send binary data
     * and is paired with receiveText. More importantly, sendText
     * is not directly multiplexed with sendBinary, meaning that
     * you can receive both text and binary messages at the same time.
     */
    sendText(message: string): void;

    /**
     * Send a binary message (Uint8Array) over the bridge.
     * 
     * sendBinary uses the binary channel to send binary data
     * and is paired with receiveBinary. More importantly, sendBinary
     * is not directly multiplexed with sendText, meaning that
     * you can receive both text and binary messages at the same time.
     */
    sendBinary(message: Uint8Array): void;

    /**
     * Receive a text message from the bridge. This method is paired with sendText.
    */
    receiveText(): Promise<string | undefined>;

    /**
     * Receive a binary message (ArrayBuffer) from the bridge. This method is paired with sendBinary.
     */
    receiveBinary(): Promise<Uint8Array | undefined>;
}
