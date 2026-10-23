using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

// System.Text.Json: same program as ../main.go (see there). Typed path uses a
// source-generated context, schema-less path uses JsonDocument/JsonElement.

public class Item
{
    [JsonPropertyName("id")] public long Id { get; set; }
    [JsonPropertyName("name")] public string Name { get; set; }
    [JsonPropertyName("price")] public double Price { get; set; }
    [JsonPropertyName("qty")] public long Qty { get; set; }
    [JsonPropertyName("active")] public bool Active { get; set; }
    [JsonPropertyName("tags")] public List<string> Tags { get; set; }
}

public class Doc
{
    [JsonPropertyName("items")] public List<Item> Items { get; set; }
}

public class Msg
{
    [JsonPropertyName("id")] public long Id { get; set; }
    [JsonPropertyName("user")] public string User { get; set; }
    [JsonPropertyName("email")] public string Email { get; set; }
    [JsonPropertyName("action")] public string Action { get; set; }
    [JsonPropertyName("ts")] public long Ts { get; set; }
    [JsonPropertyName("ok")] public bool Ok { get; set; }
    [JsonPropertyName("score")] public double Score { get; set; }
    [JsonPropertyName("tags")] public List<string> Tags { get; set; }
    [JsonPropertyName("note")] public string Note { get; set; }
}

[JsonSourceGenerationOptions(GenerationMode = JsonSourceGenerationMode.Default)]
[JsonSerializable(typeof(Doc))]
[JsonSerializable(typeof(Msg))]
public partial class Ctx : JsonSerializerContext { }

class Rng
{
    long s;
    public Rng(long s) { this.s = s; }
    public long Next()
    {
        s = (s * 1103515245L + 12345L) % 2147483648L;
        return s >> 8;
    }
}

static class Program
{
    static readonly string[] ws = {
        "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do",
        "eiusmod", "tempor", "incididunt", "ut", "labore", "et", "dolore", "magna", "aliqua", "veniam",
    };

    static string Itoa(long n) => n.ToString();

    static string Money(long cents)
    {
        long c = cents % 100;
        if (c < 10) return Itoa(cents / 100) + ".0" + Itoa(c);
        return Itoa(cents / 100) + "." + Itoa(c);
    }

    static string GenDoc(long target)
    {
        var r = new Rng(12345);
        var b = new StringBuilder((int)target + 1024);
        b.Append("{\"items\":[");
        long i = 0;
        while (b.Length < target)
        {
            if (i > 0) b.Append(",\n");
            b.Append("{\"id\":").Append(Itoa(i * 7 + 1));
            b.Append(",\"name\":\"");
            b.Append(ws[r.Next() % 20]).Append('-').Append(ws[r.Next() % 20]).Append('-').Append(Itoa(i));
            b.Append("\",\"price\":").Append(Money(r.Next() % 100000));
            b.Append(",\"qty\":").Append(Itoa(r.Next() % 1000));
            if (r.Next() % 2 == 0) b.Append(",\"active\":true,\"tags\":[");
            else b.Append(",\"active\":false,\"tags\":[");
            long nt = r.Next() % 4;
            for (long t = 0; t < nt; t++)
            {
                if (t > 0) b.Append(',');
                b.Append('"').Append(ws[r.Next() % 20]).Append('"');
            }
            b.Append("]}");
            i++;
        }
        b.Append("]}");
        return b.ToString();
    }

    static string GenMsg(Rng r, long i)
    {
        long u = r.Next() % 100000;
        var b = new StringBuilder();
        b.Append("{\"id\":").Append(Itoa(i));
        b.Append(",\"user\":\"user-").Append(Itoa(u));
        b.Append("\",\"email\":\"user").Append(Itoa(u));
        b.Append("@example.com\",\"action\":\"").Append(ws[r.Next() % 20]);
        b.Append("\",\"ts\":").Append(Itoa(1700000000 + r.Next() % 100000000));
        b.Append(",\"ok\":true,\"score\":0.").Append(Itoa(10 + r.Next() % 90));
        b.Append(",\"tags\":[\"").Append(ws[r.Next() % 20]);
        b.Append("\",\"").Append(ws[r.Next() % 20]);
        b.Append("\"],\"note\":\"").Append(ws[r.Next() % 20]);
        b.Append(' ').Append(ws[r.Next() % 20]);
        b.Append(' ').Append(ws[r.Next() % 20]);
        b.Append("\"}");
        return b.ToString();
    }

    static long Cents(double p) => (long)Math.Round(p * 100.0, MidpointRounding.AwayFromZero);

    static string Checksum(Doc d)
    {
        long sumId = 0, sumQty = 0, sumCents = 0, active = 0, tags = 0, nameBytes = 0;
        foreach (var it in d.Items)
        {
            sumId += it.Id;
            sumQty += it.Qty;
            sumCents += Cents(it.Price);
            if (it.Active) active++;
            tags += it.Tags.Count;
            nameBytes += Encoding.UTF8.GetByteCount(it.Name);
        }
        return $"items={d.Items.Count} id={sumId} qty={sumQty} cents={sumCents} active={active} tags={tags} names={nameBytes}";
    }

    // Go json.Marshal(any) shape: compact, object keys sorted, numbers as float64.
    static void WriteValue(Utf8JsonWriter w, JsonElement e)
    {
        switch (e.ValueKind)
        {
            case JsonValueKind.Object:
                var props = new List<JsonProperty>();
                foreach (var p in e.EnumerateObject()) props.Add(p);
                props.Sort((a, b) => string.CompareOrdinal(a.Name, b.Name));
                w.WriteStartObject();
                foreach (var p in props)
                {
                    w.WritePropertyName(p.Name);
                    WriteValue(w, p.Value);
                }
                w.WriteEndObject();
                break;
            case JsonValueKind.Array:
                w.WriteStartArray();
                foreach (var x in e.EnumerateArray()) WriteValue(w, x);
                w.WriteEndArray();
                break;
            case JsonValueKind.String: w.WriteStringValue(e.GetString()); break;
            case JsonValueKind.Number: w.WriteNumberValue(e.GetDouble()); break;
            case JsonValueKind.True: w.WriteBooleanValue(true); break;
            case JsonValueKind.False: w.WriteBooleanValue(false); break;
            default: w.WriteNullValue(); break;
        }
    }

    static long Ms(Stopwatch s) => s.ElapsedMilliseconds;

    static long Mbs(long bytes, Stopwatch s)
    {
        long n = (long)(s.ElapsedTicks * (1e9 / Stopwatch.Frequency));
        if (n <= 0) n = 1;
        return bytes * 1000 / n;
    }

    static void Main()
    {
        long docSize = 100000000;
        int nMsgs = 1000000;
        if (Environment.GetEnvironmentVariable("SCALE") == "small")
        {
            docSize = 1000000;
            nMsgs = 10000;
        }
        var v0 = Environment.GetEnvironmentVariable("JSON_DOC_BYTES");
        if (!string.IsNullOrEmpty(v0)) docSize = long.Parse(v0);
        string text;
        var docFile = Environment.GetEnvironmentVariable("JSON_DOC_FILE");
        if (!string.IsNullOrEmpty(docFile))
        {
            if (File.Exists(docFile)) text = File.ReadAllText(docFile);
            else
            {
                text = GenDoc(docSize);
                File.WriteAllText(docFile, text);
            }
        }
        else text = GenDoc(docSize);
        long textLen = Encoding.UTF8.GetByteCount(text);

        // 1. typed parse
        var sw = Stopwatch.StartNew();
        var doc = JsonSerializer.Deserialize(Encoding.UTF8.GetBytes(text), Ctx.Default.Doc);
        sw.Stop();
        var dParse = sw;
        string sum = Checksum(doc);
        Console.WriteLine("typed: " + sum);

        // 2. format back, check the round trip
        sw = Stopwatch.StartNew();
        byte[] outb = JsonSerializer.SerializeToUtf8Bytes(doc, Ctx.Default.Doc);
        sw.Stop();
        var dFormat = sw;
        var again = JsonSerializer.Deserialize(outb, Ctx.Default.Doc);
        Console.WriteLine("roundtrip: " + (Checksum(again) == sum ? "true" : "false"));

        // 3. schema-less value + path lookups
        sw = Stopwatch.StartNew();
        using var jd = JsonDocument.Parse(Encoding.UTF8.GetBytes(text));
        sw.Stop();
        var dValue = sw;
        sw = Stopwatch.StartNew();
        var items = jd.RootElement.GetProperty("items");
        long sumId = 0, sumCents = 0, firstTags = 0, n = 0;
        foreach (var it in items.EnumerateArray())
        {
            n++;
            sumId += (long)it.GetProperty("id").GetDouble();
            sumCents += Cents(it.GetProperty("price").GetDouble());
            var tg = it.GetProperty("tags");
            if (tg.GetArrayLength() > 0) firstTags += tg[0].GetString().Length;
        }
        sw.Stop();
        var dLookup = sw;
        Console.WriteLine($"value: {n} {sumId} {sumCents} {firstTags}");
        sw = Stopwatch.StartNew();
        var ms = new MemoryStream(1 << 20);
        using (var w = new Utf8JsonWriter(ms)) WriteValue(w, jd.RootElement);
        sw.Stop();
        var dVFormat = sw;
        Console.WriteLine("value format bytes: " + ms.Length);

        // 4. small messages
        var r = new Rng(777);
        var msgs = new string[1024];
        for (int i = 0; i < 1024; i++) msgs[i] = GenMsg(r, i);
        sw = Stopwatch.StartNew();
        long acc = 0, inBytes = 0;
        for (int i = 0; i < nMsgs; i++)
        {
            string s = msgs[i % 1024];
            inBytes += s.Length;
            var m = JsonSerializer.Deserialize(Encoding.UTF8.GetBytes(s), Ctx.Default.Msg);
            acc += m.Id + m.Ts % 1000 + m.User.Length + m.Tags.Count + m.Note.Length;
            if (m.Ok) acc++;
            acc += Cents(m.Score);
            acc += JsonSerializer.SerializeToUtf8Bytes(m, Ctx.Default.Msg).Length;
        }
        sw.Stop();
        var dMsgs = sw;
        Console.WriteLine($"messages: {nMsgs} {acc} avg bytes: {inBytes / nMsgs}");

        var e = Console.Error;
        e.WriteLine($"doc {textLen} bytes");
        e.WriteLine($"typed parse   {Ms(dParse)} ms ({Mbs(textLen, dParse)} MB/s)");
        e.WriteLine($"typed format  {Ms(dFormat)} ms ({Mbs(outb.Length, dFormat)} MB/s)");
        e.WriteLine($"value parse   {Ms(dValue)} ms ({Mbs(textLen, dValue)} MB/s)");
        e.WriteLine($"value lookups {Ms(dLookup)} ms");
        e.WriteLine($"value format  {Ms(dVFormat)} ms");
        e.WriteLine($"messages      {Ms(dMsgs)} ms ({(long)nMsgs * 1000 / (Ms(dMsgs) + 1)} msg/s, {Mbs(inBytes, dMsgs)} MB/s in)");
    }
}
