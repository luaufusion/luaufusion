/**
 * Represents the bridge interface itself
 */
interface Bridge {
    /**
     * Send a text message over the bridge
     */
    sendText(message: string): Promise<void>;

    /**
     * Send a binary message (ArrayBuffer) over the bridge
     */
    sendBinary(message: ArrayBuffer): Promise<void>;

    /**
     * Receive a text message from the bridge
    */
    receiveText(): Promise<string | undefined>;

    /**
     * Receive a binary message (ArrayBuffer) from the bridge
     */
    receiveBinary(): Promise<Uint8Array | undefined>;
}
