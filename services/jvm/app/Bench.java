// A JVM benchmark comparing Talaria and Linux on ONE AND THE SAME JDK binary.
// java.base only: no JMH, no dependencies — the Talaria image carries a trimmed JDK.
//
// Line-oriented, machine-readable output:  RESULT <test> <metric> <value> <unit>
// Usage: java Bench [all|list of tests] ; no environment variables needed.
//
// What is measured and why: throughput of JIT code (sieve/matmul), allocation
// and GC, SCALING over cores (scale), and then what Talaria exists for: thread
// wakeup latency (pingpong on LockSupport = a futex in the kernel), the cost of
// a context on a queue (queue), contention on a monitor and on CAS, and sleep
// accuracy (parkNanos/sleep).
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.*;

public class Bench {
    static final long T0 = System.nanoTime();

    public static void main(String[] a) throws Exception {
        List<String> tests = new ArrayList<>();
        if (a.length == 0 || a[0].equals("all")) tests = Arrays.asList(
            "info", "sieve", "matmul", "alloc", "scale", "pingpong", "queue", "sync", "cas", "chm", "timer");
        else tests = Arrays.asList(a);
        for (String t : tests) {
            try {
                switch (t) {
                    case "info": info(); break;
                    case "sieve": sieve(); break;
                    case "matmul": matmul(); break;
                    case "alloc": alloc(); break;
                    case "scale": scale(); break;
                    case "pingpong": pingpong(); break;
                    case "queue": queue(); break;
                    case "sync": sync(); break;
                    case "cas": cas(); break;
                    case "chm": chm(); break;
                    case "timer": timer(); break;
                    default: System.out.println("unknown test: " + t);
                }
            } catch (Throwable e) {
                System.out.println("ERROR in test " + t + ": " + e);
                e.printStackTrace(System.out);
            }
            System.out.flush();
        }
        System.out.printf("RESULT total wall %.1f ms%n", (System.nanoTime() - T0) / 1e6);
        System.out.println("BENCH DONE");
        System.out.flush();
    }

    static void res(String test, String metric, double v, String unit) {
        System.out.printf("RESULT %s %s %.3f %s%n", test, metric, v, unit);
        System.out.flush();
    }

    static void info() {
        Runtime r = Runtime.getRuntime();
        System.out.println("java " + System.getProperty("java.version") + " / " + System.getProperty("java.vm.name")
            + " / os " + System.getProperty("os.name") + " " + System.getProperty("os.arch"));
        res("info", "cores", r.availableProcessors(), "count");
        res("info", "maxheap", r.maxMemory() / 1048576.0, "MB");
        res("info", "startup", (System.nanoTime() - T0) / 1e6, "ms");
    }

    // ── single-threaded throughput ──────────────────────────────────────────
    static int sieveOnce(int n) {
        boolean[] c = new boolean[n + 1];
        int cnt = 0;
        for (int i = 2; i <= n; i++) {
            if (!c[i]) { cnt++; for (long j = (long) i * i; j <= n; j += i) c[(int) j] = true; }
        }
        return cnt;
    }
    static void sieve() {
        int n = 2_000_000, sink = 0;
        for (int i = 0; i < 3; i++) sink += sieveOnce(n);           // JIT warm-up
        long t = System.nanoTime();
        int reps = 5;
        for (int i = 0; i < reps; i++) sink += sieveOnce(n);
        double ms = (System.nanoTime() - t) / 1e6 / reps;
        res("sieve", "time", ms, "ms/pass");
        res("sieve", "rate", n * reps / ((System.nanoTime() - t) / 1e9) / 1e6, "M cells/s");
        if (sink == 0) System.out.println("(sink)");
    }

    static double[][] mat(int n, long seed) {
        Random rnd = new Random(seed);
        double[][] m = new double[n][n];
        for (int i = 0; i < n; i++) for (int j = 0; j < n; j++) m[i][j] = rnd.nextDouble();
        return m;
    }
    static double matmulOnce(double[][] x, double[][] y, double[][] z) {
        int n = x.length;
        for (int i = 0; i < n; i++) {
            double[] xi = x[i], zi = z[i];
            for (int k = 0; k < n; k++) {
                double v = xi[k]; double[] yk = y[k];
                for (int j = 0; j < n; j++) zi[j] += v * yk[j];
            }
        }
        return z[0][0];
    }
    static void matmul() {
        int n = 256;
        double[][] x = mat(n, 1), y = mat(n, 2), z = new double[n][n];
        double s = 0;
        for (int i = 0; i < 3; i++) s += matmulOnce(x, y, z);
        long t = System.nanoTime();
        int reps = 10;
        for (int i = 0; i < reps; i++) s += matmulOnce(x, y, z);
        double sec = (System.nanoTime() - t) / 1e9;
        res("matmul", "gflops", 2.0 * n * n * n * reps / sec / 1e9, "GFLOP/s");
        if (s == 12345.0) System.out.println("(sink)");
    }

    // ── allocation and GC ───────────────────────────────────────────────────
    static Object keep;
    static void alloc() {
        int n = 4_000_000;
        Object[] ring = new Object[1024];
        long t = System.nanoTime();
        for (int i = 0; i < n; i++) ring[i & 1023] = new long[8];   // 64-byte objects, almost all die young
        double sec = (System.nanoTime() - t) / 1e9;
        keep = ring;
        res("alloc", "rate", n / sec / 1e6, "M objects/s");
        res("alloc", "bytes", n * 80.0 / sec / (1 << 30), "GiB/s");
        long gcT = System.nanoTime();
        System.gc();
        res("alloc", "fullgc", (System.nanoTime() - gcT) / 1e6, "ms");
    }

    // ── scaling over cores ──────────────────────────────────────────────────
    static long cpuWork(long iters) {          // integer work that cannot be optimised to zero
        long h = 1234567;
        for (long i = 0; i < iters; i++) h = h * 6364136223846793005L + 1442695040888963407L ^ (h >>> 29);
        return h;
    }
    static double runThreads(int n, long iters) throws Exception {
        Thread[] th = new Thread[n];
        final long[] sink = new long[n];
        CountDownLatch start = new CountDownLatch(1), done = new CountDownLatch(n);
        for (int i = 0; i < n; i++) {
            final int k = i;
            th[i] = new Thread(() -> {
                try { start.await(); } catch (InterruptedException e) { return; }
                sink[k] = cpuWork(iters);
                done.countDown();
            }, "work-" + i);
            th[i].start();
        }
        Thread.sleep(50);
        long t = System.nanoTime();
        start.countDown();
        done.await();
        double sec = (System.nanoTime() - t) / 1e9;
        for (Thread x : th) x.join();
        return sec;
    }
    static void scale() throws Exception {
        long iters = 40_000_000L;
        runThreads(1, iters / 4); runThreads(2, iters / 4);          // warm-up (JIT + starting the threads)
        double base = runThreads(1, iters);
        res("scale", "1thr", base * 1000, "ms");
        int cores = Runtime.getRuntime().availableProcessors();
        for (int n = 2; n <= Math.min(8, cores); n *= 2) {
            double s = runThreads(n, iters);
            res("scale", n + "thr", s * 1000, "ms");
            // n threads do iters each: perfect scaling = the same time for
            // four times the work, i.e. speedup = n*base/s
            res("scale", "speedup" + n, n * base / s, "x");
        }
    }

    // ── thread wakeup latency (park/unpark = futex) ─────────────────────────
    static void pingpong() throws Exception {
        final int iters = 20000, warm = 2000;
        final AtomicReference<Thread> a = new AtomicReference<>(), b = new AtomicReference<>();
        final AtomicInteger turn = new AtomicInteger(0);
        final long[] rtt = new long[iters];
        Thread t2 = new Thread(() -> {
            b.set(Thread.currentThread());
            while (a.get() == null) Thread.onSpinWait();
            for (int i = 0; i < iters + warm; i++) {
                while (turn.get() != 1) LockSupport.park();
                turn.set(0);
                LockSupport.unpark(a.get());
            }
        }, "pong");
        t2.setDaemon(true);
        t2.start();
        a.set(Thread.currentThread());
        while (b.get() == null) Thread.onSpinWait();
        for (int i = 0; i < iters + warm; i++) {
            long s = System.nanoTime();
            turn.set(1);
            LockSupport.unpark(t2);
            while (turn.get() != 0) LockSupport.park();
            long d = System.nanoTime() - s;
            if (i >= warm) rtt[i - warm] = d;
        }
        Arrays.sort(rtt);
        res("pingpong", "p50", rtt[iters / 2] / 1000.0, "us RTT");
        res("pingpong", "p99", rtt[iters * 99 / 100] / 1000.0, "us RTT");
        res("pingpong", "min", rtt[0] / 1000.0, "us RTT");
    }

    // ── producer/consumer queue ─────────────────────────────────────────────
    static void queue() throws Exception {
        final int n = 200_000;
        final ArrayBlockingQueue<long[]> q = new ArrayBlockingQueue<>(1024);
        final long[] lat = new long[n];
        final CountDownLatch done = new CountDownLatch(1);
        Thread cons = new Thread(() -> {
            try {
                for (int i = 0; i < n; i++) { long[] m = q.take(); lat[i] = System.nanoTime() - m[0]; }
            } catch (InterruptedException e) { }
            done.countDown();
        }, "consumer");
        cons.setDaemon(true);
        cons.start();
        long t = System.nanoTime();
        for (int i = 0; i < n; i++) q.put(new long[]{System.nanoTime()});
        done.await();
        double sec = (System.nanoTime() - t) / 1e9;
        long[] s = lat.clone();
        Arrays.sort(s);
        res("queue", "throughput", n / sec / 1e6, "M messages/s");
        res("queue", "p50", s[n / 2] / 1000.0, "us");
        res("queue", "p99", s[n * 99 / 100] / 1000.0, "us");
    }

    // ── contention: synchronized and ReentrantLock ──────────────────────────
    static long counter;
    static final Object MON = new Object();
    static final ReentrantLock LOCK = new ReentrantLock();
    static void sync() throws Exception {
        int threads = Math.min(4, Runtime.getRuntime().availableProcessors());
        int per = 400_000;
        counter = 0;
        double sec = race(threads, () -> { for (int i = 0; i < per; i++) synchronized (MON) { counter++; } });
        res("sync", "monitor", threads * (double) per / sec / 1e6, "M acquisitions/s");
        counter = 0;
        sec = race(threads, () -> { for (int i = 0; i < per; i++) { LOCK.lock(); try { counter++; } finally { LOCK.unlock(); } } });
        res("sync", "reentrant", threads * (double) per / sec / 1e6, "M acquisitions/s");
    }
    static final AtomicLong ATOM = new AtomicLong();
    static void cas() throws Exception {
        int threads = Math.min(4, Runtime.getRuntime().availableProcessors());
        int per = 2_000_000;
        double sec = race(threads, () -> { for (int i = 0; i < per; i++) ATOM.incrementAndGet(); });
        res("cas", "atomiclong", threads * (double) per / sec / 1e6, "M increments/s");
    }
    static void chm() throws Exception {
        int threads = Math.min(4, Runtime.getRuntime().availableProcessors());
        final ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
        int per = 500_000;
        double sec = race(threads, () -> {
            int seed = (int) Thread.currentThread().getId() * 7919;
            for (int i = 0; i < per; i++) { int k = (seed + i) & 0xFFFF; m.put(k, i); if ((i & 3) == 0) m.get(k); }
        });
        res("chm", "ops", threads * (double) per / sec / 1e6, "M operations/s");
        res("chm", "size", m.size(), "keys");
    }
    static double race(int threads, Runnable body) throws Exception {
        Thread[] th = new Thread[threads];
        CountDownLatch start = new CountDownLatch(1), done = new CountDownLatch(threads);
        for (int i = 0; i < threads; i++) {
            th[i] = new Thread(() -> {
                try { start.await(); } catch (InterruptedException e) { return; }
                body.run();
                done.countDown();
            });
            th[i].start();
        }
        Thread.sleep(30);
        long t = System.nanoTime();
        start.countDown();
        done.await();
        double sec = (System.nanoTime() - t) / 1e9;
        for (Thread x : th) x.join();
        return sec;
    }

    // ── sleep and timer accuracy ────────────────────────────────────────────
    static void timer() throws Exception {
        int n = 200;
        long[] err = new long[n];
        for (int i = 0; i < n; i++) {
            long t = System.nanoTime();
            LockSupport.parkNanos(1_000_000L);                       // ask for 1 ms
            err[i] = System.nanoTime() - t - 1_000_000L;
        }
        Arrays.sort(err);
        res("timer", "park1ms_p50", err[n / 2] / 1000.0, "us overshoot");
        res("timer", "park1ms_p99", err[n * 99 / 100] / 1000.0, "us overshoot");
        long[] gr = new long[10000];
        long prev = System.nanoTime();
        for (int i = 0; i < gr.length; i++) { long c = System.nanoTime(); gr[i] = c - prev; prev = c; }
        Arrays.sort(gr);
        res("timer", "nanotime", gr[gr.length / 2] * 1.0, "ns per call");
    }
}
