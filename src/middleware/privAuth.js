const jwt = require('jsonwebtoken');
module.exports = (req, res, next) => {
    const token = req.cookies && req.cookies.authToken;
    if (!token) {
        return res.status(401).json({ error: 'No authentication token found' });
    }
    try {
        const decoded = jwt.verify(token, process.env.JWT_SECRET);
        req.user = decoded;
        next();
    } catch (error) {
        return res.status(401).json({ error: 'Invalid authentication token' });
    }
}