import { isMacOS } from "./env.utils";

export const isAppleSpeechTranscriptionSupported = (): boolean => isMacOS();
