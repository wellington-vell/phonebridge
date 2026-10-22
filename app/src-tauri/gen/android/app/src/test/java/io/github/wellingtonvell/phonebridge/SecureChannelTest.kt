package io.github.wellingtonvell.phonebridge

import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.IOException
import java.io.PipedInputStream
import java.io.PipedOutputStream
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec
import kotlin.concurrent.thread
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/** Test vectors from docs/secure-channel.md. */
class SecureChannelTest {
    private val code = "ABCDEFGHJKMNPQRSTVWX"
    private val nc = hex("000102030405060708090a0b0c0d0e0f")
    private val ns = hex("101112131415161718191a1b1c1d1e1f")
    private val keys = SecureChannel.deriveKeys(code, nc, ns)

    @Test
    fun derivedKeys() {
        assertEquals("ecf73566ebfebeb67995440cc96aadb1f1037f49fccf4418698766c1f6eeef3c", keys.authC2s.hex())
        assertEquals("eb5fc90f3f40d323dda73b7c6f0176fc54acdaf5e3c8557a9238489a8f8d50cc", keys.authS2c.hex())
        assertEquals("74432953986f9737db929a8314ebbec6127249a19c23d5d2727f20f9e7bde640", keys.keyC2s.hex())
        assertEquals("69870d929048685f2d672c69d2f2b279f6d2ddc2a4474a5e4dbbac3e89a302ab", keys.keyS2c.hex())
        assertEquals("d2ebe84b600deec4f80d13790ef361825a58e9dff0811197dd54584753205d0b", keys.keyUdp.hex())
    }

    @Test
    fun gcmVectors() {
        assertEquals("0af18ea846ffd0e5f9acc99c29b16023ffc03321a6", SecureChannel.seal(keys.keyS2c, 0, "hello".toByteArray()).hex())
        assertEquals("9e047e085752a8473d522172a55639a7fd28c505f3", SecureChannel.seal(keys.keyS2c, 1, "world".toByteArray()).hex())
    }

    @Test
    fun recordStreamMatchesVectors() {
        val wire = ByteArrayOutputStream()
        val out = SecureChannel.RecordOutputStream(wire, keys.keyS2c)
        out.write("hello".toByteArray())
        out.flush()
        out.write("world".toByteArray())
        out.flush()
        out.flush() // nothing buffered: no empty record
        assertEquals(
            "00000015" + "0af18ea846ffd0e5f9acc99c29b16023ffc03321a6" +
                "00000015" + "9e047e085752a8473d522172a55639a7fd28c505f3",
            wire.toByteArray().hex(),
        )
        val input = SecureChannel.RecordInputStream(ByteArrayInputStream(wire.toByteArray()), keys.keyS2c)
        assertEquals("helloworld", String(input.readBytes()))
    }

    @Test
    fun largeWritesSplitIntoRecords() {
        val data = ByteArray(SecureChannel.MAX_RECORD * 2 + 100) { it.toByte() }
        val wire = ByteArrayOutputStream()
        SecureChannel.RecordOutputStream(wire, keys.keyC2s).apply {
            write(data)
            flush()
        }
        assertEquals(data.size + 3 * 20, wire.size())
        val input = SecureChannel.RecordInputStream(ByteArrayInputStream(wire.toByteArray()), keys.keyC2s)
        assertArrayEquals(data, input.readBytes())
    }

    @Test
    fun tamperedRecordIsRejected() {
        val wire = ByteArrayOutputStream()
        SecureChannel.RecordOutputStream(wire, keys.keyC2s).apply {
            write("PBV1".toByteArray())
            flush()
        }
        val bytes = wire.toByteArray().also { it[6] = (it[6].toInt() xor 1).toByte() }
        expectIOException { SecureChannel.RecordInputStream(ByteArrayInputStream(bytes), keys.keyC2s).read() }
        val short = hex("00000010") + ByteArray(16)
        expectIOException { SecureChannel.RecordInputStream(ByteArrayInputStream(short), keys.keyC2s).read() }
    }

    @Test
    fun handshake() {
        assertNotNull(runHandshake(proof = { k -> k.authC2s }))
        assertNull(runHandshake(proof = { ByteArray(32) }))
    }

    @Test
    fun codes() {
        val c = SecureChannel.newCode()
        assertTrue(SecureChannel.isValidCode(c))
        val shown = SecureChannel.displayCode(c)
        assertTrue(Regex("[0-9A-Z]{5}(-[0-9A-Z]{5}){3}").matches(shown))
        assertEquals(c, SecureChannel.canonicalCode(shown.lowercase()))
    }

    /** Client side over pipes; returns the server's result (proof from [proof]). */
    private fun runHandshake(proof: (SecureChannel.Keys) -> ByteArray): SecureChannel.Streams? {
        val c2s = PipedOutputStream()
        val serverIn = PipedInputStream(c2s, 1 shl 16)
        val s2c = PipedOutputStream()
        val clientIn = DataInputStream(PipedInputStream(s2c, 1 shl 16))
        var authS2c: ByteArray? = null
        val client = thread {
            c2s.write(nc)
            c2s.flush()
            val serverNonce = ByteArray(16).also { clientIn.readFully(it) }
            val k = SecureChannel.deriveKeys(code, nc, serverNonce)
            c2s.write(proof(k))
            c2s.flush()
            val got = ByteArray(32)
            if (runCatching { clientIn.readFully(got) }.isSuccess) {
                assertArrayEquals(k.authS2c, got)
                authS2c = got
            }
        }
        val streams = SecureChannel.serverHandshake(serverIn, s2c, code)
        if (streams == null) s2c.close()
        client.join()
        assertEquals(streams != null, authS2c != null)
        return streams
    }

    @Test
    fun hkdfRfc5869Case1() {
        val ikm = ByteArray(22) { 0x0b }
        val salt = hex("000102030405060708090a0b0c")
        val prk = Mac.getInstance("HmacSHA256").run {
            init(SecretKeySpec(salt, "HmacSHA256"))
            doFinal(ikm)
        }
        val okm = SecureChannel.hkdfExpand(prk, hex("f0f1f2f3f4f5f6f7f8f9"), 42)
        assertEquals(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
            okm.hex(),
        )
    }

    private fun expectIOException(block: () -> Unit) {
        try {
            block()
            fail("expected IOException")
        } catch (_: IOException) {
        }
    }

    private fun hex(s: String) = ByteArray(s.length / 2) { s.substring(2 * it, 2 * it + 2).toInt(16).toByte() }

    private fun ByteArray.hex() = joinToString("") { "%02x".format(it) }
}
