package io.github.wellingtonvell.phonebridge

import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.security.GeneralSecurityException
import java.security.MessageDigest
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.Mac
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * PBA1 secure channel, server (phone) side: a pairing-code handshake, then
 * AES-256-GCM records in both directions. Spec: docs/secure-channel.md.
 */
object SecureChannel {
    val MAGIC = "PBA1".toByteArray()
    private const val NONCE_LEN = 16
    private const val PROOF_LEN = 32
    private const val TAG_LEN = 16
    /** Largest plaintext per record. */
    const val MAX_RECORD = 65536
    private const val ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
    private const val CODE_LEN = 20
    private val random = SecureRandom()

    /** A new pairing code, canonical form (20 Crockford base32 characters). */
    fun newCode(): String = String(CharArray(CODE_LEN) { ALPHABET[random.nextInt(ALPHABET.length)] })

    /** `ABCDE-FGHJK-MNPQR-STVWX` for a canonical code. */
    fun displayCode(code: String): String = code.chunked(5).joinToString("-")

    /** Canonical form of a displayed code: no separators, uppercase. */
    fun canonicalCode(code: String): String = code.filter { it != '-' && !it.isWhitespace() }.uppercase()

    /** A well-formed canonical code. */
    fun isValidCode(code: String): Boolean = code.length == CODE_LEN && code.all { it in ALPHABET }

    class Keys(
        val authC2s: ByteArray,
        val authS2c: ByteArray,
        val keyC2s: ByteArray,
        val keyS2c: ByteArray,
        val keyUdp: ByteArray,
    )

    /** HKDF-SHA256 of the canonical [code] with salt Nc | Ns. */
    fun deriveKeys(code: String, nc: ByteArray, ns: ByteArray): Keys {
        val prk = hmac(nc + ns, code.toByteArray(Charsets.US_ASCII))
        fun expand(label: String) = hkdfExpand(prk, "phonebridge v1 $label".toByteArray(Charsets.US_ASCII), 32)
        return Keys(expand("auth c2s"), expand("auth s2c"), expand("key c2s"), expand("key s2c"), expand("key udp"))
    }

    private fun hmac(key: ByteArray, data: ByteArray): ByteArray =
        Mac.getInstance("HmacSHA256").run {
            init(SecretKeySpec(key, "HmacSHA256"))
            doFinal(data)
        }

    /** RFC 5869 HKDF-Expand with SHA-256. */
    fun hkdfExpand(prk: ByteArray, info: ByteArray, len: Int): ByteArray {
        require(len <= 255 * 32)
        val mac = Mac.getInstance("HmacSHA256").apply { init(SecretKeySpec(prk, "HmacSHA256")) }
        val out = ByteArray(len)
        var t = ByteArray(0)
        var pos = 0
        var i = 1
        while (pos < len) {
            mac.update(t)
            mac.update(info)
            mac.update(i.toByte())
            t = mac.doFinal()
            val n = minOf(t.size, len - pos)
            System.arraycopy(t, 0, out, pos, n)
            pos += n
            i++
        }
        return out
    }

    /** GCM nonce: 4 zero bytes | counter u64 BE. */
    fun nonce(counter: Long): ByteArray = ByteArray(12).also {
        for (i in 0 until 8) it[4 + i] = (counter ushr (56 - 8 * i)).toByte()
    }

    /** Ciphertext | tag of [data] under [key] and record [counter]. */
    fun seal(key: ByteArray, counter: Long, data: ByteArray, off: Int = 0, len: Int = data.size): ByteArray =
        Cipher.getInstance("AES/GCM/NoPadding").run {
            init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_LEN * 8, nonce(counter)))
            doFinal(data, off, len)
        }

    /** The decrypted streams of an authenticated connection. */
    class Streams(val input: InputStream, val output: OutputStream)

    /**
     * The rest of the handshake once "PBA1" was read: null if the client's
     * proof is wrong (the caller delays, then closes). [code] is canonical.
     * Read timeouts come from the socket.
     */
    fun serverHandshake(input: InputStream, output: OutputStream, code: String): Streams? {
        val din = DataInputStream(input)
        val nc = ByteArray(NONCE_LEN).also { din.readFully(it) }
        val ns = ByteArray(NONCE_LEN).also { random.nextBytes(it) }
        output.write(ns)
        output.flush()
        val proof = ByteArray(PROOF_LEN).also { din.readFully(it) }
        val keys = deriveKeys(code, nc, ns)
        if (!MessageDigest.isEqual(proof, keys.authC2s)) return null
        output.write(keys.authS2c)
        output.flush()
        return Streams(RecordInputStream(input, keys.keyC2s), RecordOutputStream(output, keys.keyS2c))
    }

    /**
     * Writes [len u32 BE][ciphertext | tag] records. Buffers up to
     * [MAX_RECORD] bytes; [flush] seals what is buffered, so a writer that
     * flushes per message gets one record per message.
     */
    class RecordOutputStream(private val out: OutputStream, key: ByteArray) : OutputStream() {
        private val key = SecretKeySpec(key, "AES")
        private val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        private val buf = ByteArray(MAX_RECORD)
        private var used = 0
        private var counter = 0L

        override fun write(b: Int) {
            if (used == buf.size) seal()
            buf[used++] = b.toByte()
        }

        override fun write(b: ByteArray, off: Int, len: Int) {
            var o = off
            var n = len
            while (n > 0) {
                if (used == buf.size) seal()
                val k = minOf(n, buf.size - used)
                System.arraycopy(b, o, buf, used, k)
                used += k
                o += k
                n -= k
            }
        }

        override fun flush() {
            seal()
            out.flush()
        }

        override fun close() = out.close()

        private fun seal() {
            if (used == 0) return
            val rec = ByteArray(4 + used + TAG_LEN)
            val len = used + TAG_LEN
            rec[0] = (len ushr 24).toByte()
            rec[1] = (len ushr 16).toByte()
            rec[2] = (len ushr 8).toByte()
            rec[3] = len.toByte()
            try {
                cipher.init(Cipher.ENCRYPT_MODE, key, GCMParameterSpec(TAG_LEN * 8, nonce(counter++)))
                cipher.doFinal(buf, 0, used, rec, 4)
            } catch (e: GeneralSecurityException) {
                throw IOException("encrypt: ${e.message}", e)
            }
            used = 0
            out.write(rec)
        }
    }

    /** Reads records written by the peer; any malformed or forged record is an IOException. */
    class RecordInputStream(input: InputStream, key: ByteArray) : InputStream() {
        private val input = DataInputStream(input)
        private val key = SecretKeySpec(key, "AES")
        private val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        private var buf = ByteArray(0)
        private var pos = 0
        private var counter = 0L

        override fun read(): Int {
            if (!fill()) return -1
            return buf[pos++].toInt() and 0xff
        }

        override fun read(b: ByteArray, off: Int, len: Int): Int {
            if (len == 0) return 0
            if (!fill()) return -1
            val n = minOf(len, buf.size - pos)
            System.arraycopy(buf, pos, b, off, n)
            pos += n
            return n
        }

        override fun available() = buf.size - pos

        override fun close() = input.close()

        /** False at a clean end of stream between records. */
        private fun fill(): Boolean {
            if (pos < buf.size) return true
            val first = input.read()
            if (first < 0) return false
            val len = (first shl 24) or (input.readUnsignedByte() shl 16) or
                (input.readUnsignedByte() shl 8) or input.readUnsignedByte()
            if (len < TAG_LEN + 1 || len > MAX_RECORD + TAG_LEN) throw IOException("bad record length $len")
            val sealed = ByteArray(len)
            try {
                input.readFully(sealed)
            } catch (e: EOFException) {
                throw IOException("truncated record", e)
            }
            buf = try {
                cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_LEN * 8, nonce(counter++)))
                cipher.doFinal(sealed)
            } catch (e: GeneralSecurityException) {
                throw IOException("record authentication failed", e)
            }
            pos = 0
            return true
        }
    }
}
