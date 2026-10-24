using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Threading.Channels;

// .NET 10 ports of the async benchmarks (reference: ../<name>/main.go).
// Usage: bench <name>   (SCALE=small for the smoke workload). Unbuffered Go
// channels become Channel.CreateBounded(1) (.NET has no rendezvous channel).
static class P
{
    static readonly bool Small = Environment.GetEnvironmentVariable("SCALE") == "small";
    static long Ms(long t0, long t1) => (long)Stopwatch.GetElapsedTime(t0, t1).TotalMilliseconds;
    static long Now => Stopwatch.GetTimestamp();

    static Channel<int> Bounded(int cap) => Channel.CreateBounded<int>(new BoundedChannelOptions(cap) { FullMode = BoundedChannelFullMode.Wait });

    static async Task Main(string[] a)
    {
        switch (a[0])
        {
            case "spawn_tasks": await SpawnTasks(); break;
            case "channel_pingpong": await PingPong(); break;
            case "fanout_fanin": await FanOut(); break;
            case "mutex_contention": await Mutex(); break;
            case "timers": await Timers(); break;
            case "await_chain": await AwaitChain(); break;
            case "cancel_tree": await CancelTree(); break;
            case "async_io": await AsyncIo(); break;
            case "select_many": await SelectMany(); break;
        }
    }

    static async Task SpawnTasks()
    {
        int n = Small ? 10000 : 1000000;
        var res = new int[n];
        long t0 = Now;
        var tasks = new Task[n];
        for (int i = 0; i < n; i++) { int j = i; tasks[i] = Task.Run(() => { res[j] = (j * 7 + 3) % 1000; }); }
        await Task.WhenAll(tasks);
        long sum = 0; foreach (var v in res) sum += v;
        long t1 = Now;
        // park: async tasks suspended on a shared gate (TCS), the analogue of goroutines parked on a channel
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        int started = 0;
        var startedTcs = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        async Task Parked(int j)
        {
            if (Interlocked.Increment(ref started) == n) startedTcs.SetResult();
            await gate.Task;
            res[j] = j % 1000;
        }
        for (int i = 0; i < n; i++) { int j = i; tasks[i] = Task.Run(() => Parked(j)); }
        await startedTcs.Task;
        long t2 = Now;
        gate.SetResult();
        await Task.WhenAll(tasks);
        foreach (var v in res) sum += v;
        long t3 = Now;
        Console.WriteLine($"spawn_tasks {sum}");
        Console.WriteLine($"  run1M_ms {Ms(t0, t1)} park1M_ms {Ms(t1, t2)} wake1M_ms {Ms(t2, t3)}");
    }

    static async Task Ponger(ChannelReader<int> i, ChannelWriter<int> o)
    {
        await foreach (var v in i.ReadAllAsync()) await o.WriteAsync(v + 1);
        o.Complete();
    }

    static async Task<long> PingPongRun(int msgs)
    {
        var a = Bounded(1); var b = Bounded(1);
        var p = Task.Run(() => Ponger(a.Reader, b.Writer));
        long sum = 0;
        for (int i = 0; i < msgs / 2; i++) { await a.Writer.WriteAsync(i); sum += await b.Reader.ReadAsync(); }
        a.Writer.Complete();
        await foreach (var _ in b.Reader.ReadAllAsync()) { }
        await p;
        return sum;
    }

    static async Task Stage(ChannelReader<int> i, ChannelWriter<int> o, int kind)
    {
        await foreach (var v in i.ReadAllAsync())
            await o.WriteAsync(kind == 0 ? v + 1 : kind == 1 ? v * 3 : v ^ 0x5555);
        o.Complete();
    }

    static async Task<long> Pipeline(int n)
    {
        var c0 = Bounded(128); var c1 = Bounded(128); var c2 = Bounded(128); var c3 = Bounded(128);
        _ = Task.Run(async () => { for (int i = 0; i < n; i++) await c0.Writer.WriteAsync(i); c0.Writer.Complete(); });
        _ = Task.Run(() => Stage(c0.Reader, c1.Writer, 0));
        _ = Task.Run(() => Stage(c1.Reader, c2.Writer, 1));
        _ = Task.Run(() => Stage(c2.Reader, c3.Writer, 2));
        long sum = 0;
        await foreach (var v in c3.Reader.ReadAllAsync()) sum += v;
        return sum;
    }

    static async Task PingPong()
    {
        int msgs = Small ? 50000 : 5000000, items = Small ? 10000 : 1000000;
        long t0 = Now; var a = await PingPongRun(msgs);
        long t1 = Now; var b = await Pipeline(items);
        long t2 = Now;
        Console.WriteLine($"channel_pingpong {a} {b}");
        Console.WriteLine($"  pingpong_ms {Ms(t0, t1)} pipeline_ms {Ms(t1, t2)}");
    }

    static async Task<long> Fanin(int producers, int each)
    {
        var ch = Bounded(1024);
        var ps = new Task[producers];
        for (int p = 0; p < producers; p++)
        {
            int id = p;
            ps[p] = Task.Run(async () => { for (int i = 0; i < each; i++) await ch.Writer.WriteAsync(id * each + i); });
        }
        _ = Task.Run(async () => { await Task.WhenAll(ps); ch.Writer.Complete(); });
        long sum = 0;
        await foreach (var v in ch.Reader.ReadAllAsync()) sum += v;
        return sum;
    }

    static int Job(int x)
    {
        long h = x;
        for (int i = 0; i < 8; i++) h = (h * 1103515245 + 12345) % 2147483648;
        return (int)(h % 1000);
    }

    static async Task<long> Pool(int workers, int n)
    {
        var jobs = Bounded(1024); var results = Bounded(1024);
        var ws = new Task[workers];
        for (int w = 0; w < workers; w++)
            ws[w] = Task.Run(async () => { await foreach (var j in jobs.Reader.ReadAllAsync()) await results.Writer.WriteAsync(Job(j)); });
        var done = Task.Run(async () => { long s = 0; await foreach (var v in results.Reader.ReadAllAsync()) s += v; return s; });
        _ = Task.Run(async () => { await Task.WhenAll(ws); results.Writer.Complete(); });
        for (int i = 0; i < n; i++) await jobs.Writer.WriteAsync(i);
        jobs.Writer.Complete();
        return await done;
    }

    static async Task FanOut()
    {
        int producers = Small ? 100 : 10000, jobs = Small ? 10000 : 1000000;
        long t0 = Now; var a = await Fanin(producers, 100);
        long t1 = Now; var b = await Pool(8, jobs);
        long t2 = Now;
        Console.WriteLine($"fanout_fanin {a} {b}");
        Console.WriteLine($"  fanin_ms {Ms(t0, t1)} pool_ms {Ms(t1, t2)}");
    }

    static async Task Mutex()
    {
        int total = Small ? 100000 : 10000000;
        long c = 0; var sem = new SemaphoreSlim(1, 1);
        long t0 = Now;
        var ts = new Task[8];
        for (int k = 0; k < 8; k++)
            ts[k] = Task.Run(async () => { for (int i = 0; i < total / 8; i++) { await sem.WaitAsync(); c++; sem.Release(); } });
        await Task.WhenAll(ts);
        long t1 = Now;
        long u = 0;
        for (int i = 0; i < total; i++) { await sem.WaitAsync(); u++; sem.Release(); }
        long t2 = Now;
        long l = 0; object o = new();
        for (int i = 0; i < total; i++) { lock (o) { l++; } }
        long t3 = Now;
        Console.WriteLine($"mutex_contention {c} {u}");
        Console.WriteLine($"  contended_ms {Ms(t0, t1)} uncontended_ms {Ms(t1, t2)} uncontended_lock_ms {Ms(t2, t3)}");
    }

    static async Task Timers()
    {
        int n = Small ? 1000 : 100000;
        long seed = 12345; var late = new long[n]; long sum = 0;
        var ts = new Task[n];
        long t0 = Now;
        for (int i = 0; i < n; i++)
        {
            seed = (seed * 1103515245 + 12345) % 2147483648;
            int ms = (int)(1 + seed / 65536 % 10); sum += ms; int j = i;
            ts[i] = Task.Run(async () =>
            {
                long s = Now;
                await Task.Delay(ms);
                late[j] = (long)Stopwatch.GetElapsedTime(s).TotalNanoseconds - ms * 1000000L;
            });
        }
        long t1 = Now;
        await Task.WhenAll(ts);
        long t2 = Now;
        long maxLate = 0, totalLate = 0; int over = 0;
        foreach (var l in late) { if (l > maxLate) maxLate = l; if (l > 1000000) over++; totalLate += l; }
        Console.WriteLine($"timers {n} {sum}");
        Console.WriteLine($"  start_ms {Ms(t0, t1)} total_ms {Ms(t0, t2)} max_late_us {maxLate / 1000} mean_late_us {totalLate / n / 1000} late_over_1ms {over}");
    }

    static async Task<int> Chain(int d, int x) => d == 0 ? x & 1023 : await Chain(d - 1, x + d) + 1;
    static async Task<int> ChainYield(int d, int x)
    {
        if (d == 0) { await Task.Yield(); return x & 1023; }
        return await ChainYield(d - 1, x + d) + 1;
    }

    static async Task AwaitChain()
    {
        int outer = Small ? 100 : 10000, outerYield = Small ? 10 : 1000;
        long t0 = Now; long a = 0;
        for (int i = 0; i < outer; i++) a += await Chain(1000, i);
        long t1 = Now; long b = 0;
        for (int i = 0; i < outerYield; i++) b += await ChainYield(1000, i);
        long t2 = Now;
        Console.WriteLine($"await_chain {a} {b}");
        Console.WriteLine($"  sync_ms {Ms(t0, t1)} yield_ms {Ms(t1, t2)}");
    }

    static async Task<int> Waiter(CancellationToken ct, Channel<int> ch, int i)
    {
        try
        {
            if (i % 2 == 0) { await ch.Reader.ReadAsync(ct); return 0; }
            await Task.Delay(TimeSpan.FromSeconds(100), ct); return 0;
        }
        catch (OperationCanceledException) { return 1; }
    }

    static async Task<int> StartAll(CancellationToken ct, Channel<int> ch, int n)
    {
        var ts = new Task<int>[n];
        for (int i = 0; i < n; i++) { int j = i; ts[j] = Task.Run(() => Waiter(ct, ch, j)); }
        int c = 0; foreach (var r in await Task.WhenAll(ts)) c += r;
        return c;
    }

    static async Task CancelTree()
    {
        int n = Small ? 1000 : 10000, rounds = Small ? 2 : 10;
        var ch = Bounded(1);
        int cancelled = 0; double drain = 0, maxDrain = 0;
        for (int r = 0; r < rounds; r++)
        {
            using var cts = new CancellationTokenSource();
            var done = Task.Run(() => StartAll(cts.Token, ch, n));
            await Task.Delay(20);
            long c0 = Now;
            cts.Cancel();
            cancelled += await done;
            double d = Stopwatch.GetElapsedTime(c0).TotalNanoseconds;
            drain += d; if (d > maxDrain) maxDrain = d;
        }
        double timeout = 0;
        for (int r = 0; r < rounds; r++)
        {
            long s0 = Now;
            using var cts = new CancellationTokenSource(20);
            cancelled += await StartAll(cts.Token, ch, n);
            timeout += Stopwatch.GetElapsedTime(s0).TotalNanoseconds - 20e6;
        }
        Console.WriteLine($"cancel_tree {cancelled}");
        Console.WriteLine($"  cancel_drain_us_avg {(long)(drain / rounds) / 1000} max {(long)maxDrain / 1000} timeout_drain_us_avg {(long)(timeout / rounds) / 1000}");
    }

    static async Task Serve(Socket s)
    {
        var buf = new byte[4096];
        try
        {
            while (true)
            {
                int n = await s.ReceiveAsync(buf, SocketFlags.None);
                if (n == 0) break;
                int off = 0;
                while (off < n) off += await s.SendAsync(buf.AsMemory(off, n - off), SocketFlags.None);
            }
        }
        catch (SocketException) { }
        s.Dispose();
    }

    static async Task<int> Client(EndPoint addr, int trips, long[] lat, int off)
    {
        var c = new Socket(SocketType.Stream, ProtocolType.Tcp) { NoDelay = true };
        await c.ConnectAsync(addr);
        var msg = new byte[64]; Array.Fill(msg, (byte)'p'); var buf = new byte[64];
        int got = 0;
        for (int k = 0; k < trips; k++)
        {
            long t0 = Now;
            await c.SendAsync(msg, SocketFlags.None);
            int r = 0;
            while (r < 64) r += await c.ReceiveAsync(buf.AsMemory(r), SocketFlags.None);
            lat[off + k] = (long)Stopwatch.GetElapsedTime(t0).TotalNanoseconds;
            got += 64;
        }
        c.Dispose();
        return got;
    }

    static async Task AsyncIo()
    {
        int conns = Small ? 50 : 1000, trips = Small ? 50 : 1000;
        var l = new Socket(SocketType.Stream, ProtocolType.Tcp);
        l.Bind(new IPEndPoint(IPAddress.Loopback, 0)); l.Listen(4096);
        var addr = l.LocalEndPoint!;
        _ = Task.Run(async () =>
        {
            for (int i = 0; i < conns; i++)
            {
                var c = await l.AcceptAsync(); c.NoDelay = true;
                _ = Task.Run(() => Serve(c));
            }
        });
        var lat = new long[conns * trips];
        long t0 = Now;
        var ts = new Task<int>[conns];
        for (int i = 0; i < conns; i++) { int j = i; ts[j] = Task.Run(() => Client(addr, trips, lat, j * trips)); }
        long total = 0; foreach (var r in await Task.WhenAll(ts)) total += r;
        long ns = (long)Stopwatch.GetElapsedTime(t0).TotalNanoseconds;
        l.Dispose();
        Array.Sort(lat);
        long n = lat.Length;
        Console.WriteLine($"async_io {total}");
        Console.WriteLine($"  total_ms {ns / 1000000} rt_per_s {n * 1000 / (ns / 1000000)} p50_us {lat[n / 2] / 1000} p99_us {lat[n * 99 / 100] / 1000} max_us {lat[n - 1] / 1000}");
    }

    static async Task SelectMany()
    {
        int k = 8, each = Small ? 2500 : 250000;
        async Task Producer(ChannelWriter<int> w, int id)
        {
            for (int i = 0; i < each; i++) await w.WriteAsync(id * 1000000 + i);
            w.Complete();
        }
        var chs = new Channel<int>[k];
        for (int i = 0; i < k; i++) chs[i] = Bounded(1);
        long t0 = Now;
        for (int i = 0; i < k; i++) { int id = i; _ = Task.Run(() => Producer(chs[id].Writer, id)); }
        // one pending read per channel, Task.WhenAny re-armed for the winner (as main.zl's task.Any)
        var pending = new Task<int>[k];
        var open = new bool[k];
        for (int i = 0; i < k; i++) { pending[i] = chs[i].Reader.ReadAsync().AsTask(); open[i] = true; }
        long a = 0, got = 0, total = (long)k * each;
        while (got < total)
        {
            var w = await Task.WhenAny(pending);
            int idx = Array.IndexOf(pending, w);
            try { a += await w; got++; pending[idx] = chs[idx].Reader.ReadAsync().AsTask(); }
            catch (ChannelClosedException) { pending[idx] = new TaskCompletionSource<int>().Task; }
        }
        long t1 = Now;
        var chs2 = new Channel<int>[k];
        for (int i = 0; i < k; i++) chs2[i] = Bounded(1);
        var merged = Bounded(1);
        var fw = new Task[k];
        for (int i = 0; i < k; i++)
        {
            int id = i;
            _ = Task.Run(() => Producer(chs2[id].Writer, id));
            fw[i] = Task.Run(async () => { await foreach (var v in chs2[id].Reader.ReadAllAsync()) await merged.Writer.WriteAsync(v); });
        }
        _ = Task.Run(async () => { await Task.WhenAll(fw); merged.Writer.Complete(); });
        long b = 0;
        await foreach (var v in merged.Reader.ReadAllAsync()) b += v;
        long t2 = Now;
        Console.WriteLine($"select_many {a} {b}");
        Console.WriteLine($"  select_ms {Ms(t0, t1)} merge_ms {Ms(t1, t2)}");
    }
}
